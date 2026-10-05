//! A small Debug Adapter Protocol client: the adapter (lldb-dap, debugpy...) runs as a child
//! process and speaks `Content-Length`-framed JSON over its stdio, like a language server, or
//! connects back over TCP (Delve), or runs in-process (our Node adapter).
//!
//! Reading and writing happen on background threads so a slow adapter never blocks the UI.
//! Incoming messages are queued and the `waker` callback is invoked so the UI can `poll`.

mod types;

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

pub use types::*;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
pub enum Incoming {
    /// The answer to the request numbered `request_seq`.
    Response { request_seq: i64, command: String, result: Result<Value, String> },
    Event { event: String, body: Value },
    /// A request from the adapter (`runInTerminal`, `startDebugging`), answered with
    /// `Client::respond`.
    Request { seq: i64, command: String, arguments: Value },
    /// A line the adapter wrote to stderr.
    Log(String),
    Exited,
}

pub struct Client {
    /// The adapter's capabilities, from its answer to `initialize` (Null until then).
    pub capabilities: Value,
    /// The adapter process (None for an adapter running in-process).
    child: Option<Child>,
    out: Sender<Value>,
    rx: Receiver<Incoming>,
    next_seq: i64,
    exited: bool,
}

/// How long `spawn_client_addr` waits for the adapter to connect.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl Client {
    /// Starts the adapter `command` in `cwd`, speaking over its stdio, and sends `initialize`
    /// (as request 1).
    pub fn spawn(command: &Path, args: &[String], cwd: &Path, adapter_id: &str, waker: Waker) -> io::Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let out = start_stream(Box::new(stdout), Box::new(stdin), tx.clone(), waker.clone())?;
        forward_lines(stderr, None, tx, waker)?;
        Ok(Self::started(Some(child), out, rx, adapter_id))
    }

    /// Starts an adapter that connects back to us over TCP (`dlv dap --client-addr`): `{addr}`
    /// in `args` becomes the address we listen on. The adapter's stdout (the program's output)
    /// arrives as `output` events, its stderr as `Incoming::Log`.
    pub fn spawn_client_addr(command: &Path, args: &[String], cwd: &Path, adapter_id: &str, waker: Waker) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?.to_string();
        let args: Vec<String> = args.iter().map(|a| a.replace("{addr}", &addr)).collect();
        let mut child = Command::new(command)
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (tx, rx) = mpsc::channel();
        // Delve's stdout is the program's.
        forward_lines(child.stdout.take().unwrap(), Some("stdout"), tx.clone(), waker.clone())?;
        forward_lines(child.stderr.take().unwrap(), None, tx.clone(), waker.clone())?;
        // Messages queue up until the adapter has connected.
        let (out, out_rx) = mpsc::channel::<Value>();
        thread::Builder::new().name("dap connect".into()).spawn(move || {
            let _ = listener.set_nonblocking(true);
            let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break Some(stream),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                        thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break None,
                }
            };
            let Some(stream) = stream.and_then(|s| s.set_nonblocking(false).ok().map(|_| s)) else {
                let _ = tx.send(Incoming::Log("The debug adapter didn't connect.".into()));
                let _ = tx.send(Incoming::Exited);
                return waker();
            };
            let Ok(reader) = stream.try_clone() else { return };
            let Ok(inner) = start_stream(Box::new(reader), Box::new(stream), tx, waker) else { return };
            for msg in out_rx {
                if inner.send(msg).is_err() {
                    return;
                }
            }
        })?;
        Ok(Self::started(Some(child), out, rx, adapter_id))
    }

    /// Connects to an adapter that is already listening (`dlv --headless --listen=host:port`).
    pub fn connect(addr: &str, adapter_id: &str, waker: Waker) -> io::Result<Self> {
        let stream = std::net::TcpStream::connect(addr)?;
        let (tx, rx) = mpsc::channel();
        let out = start_stream(Box::new(stream.try_clone()?), Box::new(stream), tx, waker)?;
        Ok(Self::started(None, out, rx, adapter_id))
    }

    /// Runs an adapter inside the editor: `serve` gets the requests and sends responses and
    /// events back (unframed DAP messages), on a thread of its own. It should return when its
    /// receiver closes.
    pub fn in_process(adapter_id: &str, waker: Waker, serve: impl FnOnce(Receiver<Value>, Sender<Value>) + Send + 'static) -> io::Result<Self> {
        let (out, adapter_rx) = mpsc::channel::<Value>();
        let (adapter_tx, from_adapter) = mpsc::channel::<Value>();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name(format!("{adapter_id} adapter")).spawn(move || serve(adapter_rx, adapter_tx))?;
        thread::Builder::new().name("dap in-process".into()).spawn(move || {
            for msg in from_adapter {
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
        Ok(Self::started(None, out, rx, adapter_id))
    }

    fn started(child: Option<Child>, out: Sender<Value>, rx: Receiver<Incoming>, adapter_id: &str) -> Self {
        let mut client = Self { capabilities: Value::Null, child, out, rx, next_seq: 1, exited: false };
        client.request(
            "initialize",
            json!({
                "clientID": "orbvane",
                "clientName": "orbvane",
                "adapterID": adapter_id,
                "locale": "en",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsVariablePaging": false,
                "supportsRunInTerminalRequest": false,
                "supportsMemoryReferences": false,
                "supportsProgressReporting": false,
                "supportsInvalidatedEvent": true,
            }),
        );
        client
    }

    /// Sends a request and returns its number; the response arrives later via `poll`.
    pub fn request(&mut self, command: &str, arguments: Value) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut msg = json!({ "seq": seq, "type": "request", "command": command });
        if !arguments.is_null() {
            msg["arguments"] = arguments;
        }
        let _ = self.out.send(msg);
        seq
    }

    /// Answers a request the adapter sent (see `Incoming::Request`).
    pub fn respond(&mut self, request_seq: i64, command: &str, success: bool, body: Value) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let _ = self.out.send(json!({
            "seq": seq, "type": "response", "request_seq": request_seq,
            "command": command, "success": success, "body": body,
        }));
    }

    /// Drains incoming messages. The `initialize` answer also fills in `capabilities`.
    pub fn poll(&mut self) -> Vec<Incoming> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match &msg {
                Incoming::Response { command, result: Ok(body), .. } if command == "initialize" => {
                    self.capabilities = body.clone();
                }
                Incoming::Exited => self.exited = true,
                _ => {}
            }
            out.push(msg);
        }
        out
    }

    /// Whether the adapter supports an optional feature (`supportsConfigurationDoneRequest`...).
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities[capability].as_bool().unwrap_or(false)
    }

    /// Stops the adapter process (after `disconnect` had its chance).
    pub fn kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // An in-process adapter ends when its requests stop.
        let (dead, _) = mpsc::channel();
        self.out = dead;
        self.exited = true;
    }

    pub fn exited(&self) -> bool {
        self.exited
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.exited {
            self.kill();
        }
    }
}

/// Speaks framed DAP over a byte stream: a writer thread for the returned sender, and a reader
/// thread that queues what arrives (then `Incoming::Exited`).
fn start_stream(read: Box<dyn io::Read + Send>, write: Box<dyn io::Write + Send>, tx: Sender<Incoming>, waker: Waker) -> io::Result<Sender<Value>> {
    // ORBVANE_DAP_LOG=<file> records the conversation.
    let log = std::env::var_os("ORBVANE_DAP_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()).map(|f| Arc::new(std::sync::Mutex::new(f)));
    let reader_log = log.clone();
    let (out, out_rx) = mpsc::channel::<Value>();
    thread::Builder::new().name("dap writer".into()).spawn(move || {
        let mut stdin = io::BufWriter::new(write);
        for msg in out_rx {
            let body = msg.to_string();
            if let Some(log) = &log {
                let _ = writeln!(log.lock().unwrap(), "-> {body}");
            }
            let ok = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).and_then(|_| stdin.flush());
            if ok.is_err() {
                break;
            }
        }
    })?;
    thread::Builder::new().name("dap reader".into()).spawn(move || {
        let mut reader = BufReader::new(read);
        while let Some(msg) = read_message(&mut reader) {
            if let Some(log) = &reader_log {
                let _ = writeln!(log.lock().unwrap(), "<- {msg}");
            }
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
    Ok(out)
}

/// Queues each line of `stream` as `Incoming::Log`, or as an `output` event of `category`.
fn forward_lines(stream: impl io::Read + Send + 'static, category: Option<&'static str>, tx: Sender<Incoming>, waker: Waker) -> io::Result<()> {
    thread::Builder::new().name("dap output".into()).spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let msg = match category {
                Some(category) => Incoming::Event { event: "output".into(), body: json!({ "category": category, "output": format!("{line}\n") }) },
                None => Incoming::Log(line),
            };
            if tx.send(msg).is_err() {
                return;
            }
            waker();
        }
    })?;
    Ok(())
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
    let text = |k: &str| msg[k].as_str().unwrap_or_default().to_string();
    match msg["type"].as_str()? {
        "response" => {
            let result = if msg["success"].as_bool().unwrap_or(false) {
                Ok(msg["body"].clone())
            } else {
                // The error's own text when there is one (`body.error.format`), else `message`.
                let format = msg["body"]["error"]["format"].as_str().map(String::from);
                Err(format.unwrap_or_else(|| text("message")))
            };
            Some(Incoming::Response { request_seq: msg["request_seq"].as_i64()?, command: text("command"), result })
        }
        "event" => Some(Incoming::Event { event: text("event"), body: msg["body"].clone() }),
        "request" => Some(Incoming::Request { seq: msg["seq"].as_i64()?, command: text("command"), arguments: msg["arguments"].clone() }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
    }

    #[test]
    fn reads_responses_events_and_requests() {
        let raw = [
            frame(r#"{"seq":1,"type":"response","request_seq":4,"command":"threads","success":true,"body":{"threads":[]}}"#),
            frame(r#"{"seq":2,"type":"response","request_seq":5,"command":"evaluate","success":false,"message":"nope"}"#),
            frame(r#"{"seq":3,"type":"event","event":"stopped","body":{"reason":"breakpoint","threadId":7}}"#),
            frame(r#"{"seq":4,"type":"request","command":"runInTerminal","arguments":{"args":["a"]}}"#),
        ]
        .concat();
        let mut reader = BufReader::new(raw.as_bytes());
        let mut got = Vec::new();
        while let Some(m) = read_message(&mut reader) {
            got.push(classify(m).unwrap());
        }
        assert!(matches!(&got[0], Incoming::Response { request_seq: 4, command, result: Ok(_) } if command == "threads"));
        assert!(matches!(&got[1], Incoming::Response { request_seq: 5, result: Err(e), .. } if e == "nope"));
        assert!(matches!(&got[2], Incoming::Event { event, body } if event == "stopped" && body["threadId"] == 7));
        assert!(matches!(&got[3], Incoming::Request { seq: 4, command, .. } if command == "runInTerminal"));
    }

    #[test]
    fn error_text_prefers_the_formatted_error() {
        let m = serde_json::from_str(r#"{"type":"response","request_seq":1,"command":"launch","success":false,"message":"x","body":{"error":{"format":"no program"}}}"#).unwrap();
        assert!(matches!(classify(m), Some(Incoming::Response { result: Err(e), .. }) if e == "no program"));
    }
}
