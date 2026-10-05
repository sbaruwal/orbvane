//! The debug adapter: DAP requests from the editor become inspector (CDP) calls, inspector
//! events become DAP events. One thread handles everything in order; a CDP call waits for its
//! answer and queues whatever else arrives meanwhile.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::format;
use crate::sourcemap::{normalize, SourceMap};
use crate::ws;

/// How long a CDP call or the inspector's start may take.
const TIMEOUT: Duration = Duration::from_secs(15);
/// Node runs JavaScript on one thread; workers aren't debugged.
const THREAD_ID: i64 = 1;

enum Input {
    Dap(Value),
    DapClosed,
    Cdp(Value),
    CdpClosed,
    /// A line the program wrote ("stdout" or "stderr").
    Output(&'static str, String),
    Exited(Option<i32>),
}

struct Script {
    url: String,
    path: Option<PathBuf>,
    map_url: String,
    /// Loaded on first use (None inside: there's no usable map).
    map: Option<Option<Arc<SourceMap>>>,
}

#[derive(Clone)]
struct Bp {
    id: i64,
    /// 0-based.
    line: u32,
    column: Option<u32>,
    /// The CDP condition (the user's condition, hit count and log message folded together).
    condition: String,
}

/// What a `variablesReference` stands for.
enum Handle {
    /// A scope's object; the local scope also shows `this`.
    Scope { object: String, this: Option<Value> },
    Object(String),
}

struct Adapter {
    tx: Sender<Value>,
    seq: i64,
    inputs: Receiver<Input>,
    input_tx: Sender<Input>,
    /// Inputs that arrived while a CDP call waited.
    queue: VecDeque<Input>,
    ws: Option<ws::Writer>,
    cdp_id: i64,
    /// The program we launched (None when attached).
    child: Option<Arc<Mutex<Child>>>,
    thread_name: String,
    no_debug: bool,
    stop_on_entry: bool,
    source_maps: bool,
    /// `"outputCapture": "std"`: the program's stdout/stderr instead of console calls.
    capture_std: bool,
    blackbox: Vec<String>,
    scripts: HashMap<String, Script>,
    /// Breakpoints the editor wants, by file.
    desired: HashMap<PathBuf, Vec<Bp>>,
    /// The inspector's breakpoint ids for each file's breakpoints.
    cdp_bps: HashMap<PathBuf, Vec<String>>,
    next_bp: i64,
    /// The call frames of the current pause.
    frames: Vec<Value>,
    paused: Option<Value>,
    handles: Vec<Handle>,
    /// Script ids behind `sourceReference`s.
    source_refs: Vec<String>,
    /// The reason to report for the next pause (a step, a pause request).
    expect: Option<&'static str>,
    /// The program's first pause (`--inspect-brk`) is still to come.
    entry_pending: bool,
    main_context: Option<i64>,
    terminated: bool,
}

/// Runs the adapter until the editor goes away or disconnects (`dap::Client::in_process`).
pub fn serve(rx: Receiver<Value>, tx: Sender<Value>) {
    let (input_tx, inputs) = mpsc::channel();
    let forward = input_tx.clone();
    thread::spawn(move || {
        for msg in rx {
            if forward.send(Input::Dap(msg)).is_err() {
                return;
            }
        }
        let _ = forward.send(Input::DapClosed);
    });
    let mut a = Adapter {
        tx,
        seq: 0,
        inputs,
        input_tx,
        queue: VecDeque::new(),
        ws: None,
        cdp_id: 0,
        child: None,
        thread_name: "Main Thread".into(),
        no_debug: false,
        stop_on_entry: false,
        source_maps: true,
        capture_std: false,
        blackbox: Vec::new(),
        scripts: HashMap::new(),
        desired: HashMap::new(),
        cdp_bps: HashMap::new(),
        next_bp: 0,
        frames: Vec::new(),
        paused: None,
        handles: Vec::new(),
        source_refs: Vec::new(),
        expect: None,
        entry_pending: false,
        main_context: None,
        terminated: false,
    };
    a.run();
    a.shutdown();
}

fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsConditionalBreakpoints": true,
        "supportsHitConditionalBreakpoints": true,
        "supportsLogPoints": true,
        "supportsEvaluateForHovers": true,
        "supportsTerminateRequest": true,
        "supportsExceptionInfoRequest": true,
        "exceptionBreakpointFilters": [
            { "filter": "all", "label": "Caught Exceptions", "default": false },
            { "filter": "uncaught", "label": "Uncaught Exceptions", "default": false },
        ],
    })
}

/// Lines the inspector prints that the console shouldn't show.
fn inspector_chatter(line: &str) -> bool {
    ["Debugger listening on ", "For help, see: https://nodejs.org", "Debugger attached.", "Waiting for the debugger to disconnect", "Debugger ending on "]
        .iter()
        .any(|p| line.starts_with(p))
}

fn escape_regex(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// Matches a file's script URL in any of the forms Node uses.
fn url_regex(path: &Path) -> String {
    let plain = path.to_string_lossy().to_string();
    let encoded = crate::percent_encode(&plain);
    let forms = if encoded == plain { escape_regex(&plain) } else { format!("{}|{}", escape_regex(&plain), escape_regex(&encoded)) };
    format!("^(?:file://)?(?:{forms})$")
}

/// `skipFiles` globs as inspector blackbox patterns.
fn blackbox_pattern(glob: &str) -> String {
    if let Some(rest) = glob.strip_prefix("<node_internals>") {
        let _ = rest;
        return "^node:|^internal/".into();
    }
    let mut out = String::from("^(?:file://)?");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                out.push_str(".*");
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            c => out.push_str(&escape_regex(&c.to_string())),
        }
    }
    out.push('$');
    out
}

/// A log message (`x is {x}`) as a template literal.
fn log_template(message: &str) -> String {
    let mut out = String::from("`");
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut depth = 1;
                let mut expr = String::new();
                for c in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        break;
                    }
                    expr.push(c);
                }
                out.push_str(&format!("${{{expr}}}"));
            }
            '`' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('`');
    out
}

/// The inspector condition for a DAP breakpoint: the condition, then the hit count (counted
/// in a global of ours), then the log message (logged, never pausing).
fn breakpoint_condition(id: i64, bp: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(c) = bp["condition"].as_str().filter(|c| !c.trim().is_empty()) {
        parts.push(format!("({c})"));
    }
    if let Some(h) = bp["hitCondition"].as_str().map(str::trim).filter(|h| !h.is_empty()) {
        let (op, n) = match h.find(|c: char| c.is_ascii_digit()) {
            Some(i) => (h[..i].trim(), h[i..].trim()),
            None => ("", h),
        };
        let count = format!("((globalThis.__orbvaneHits ??= {{}})[{id}] = (globalThis.__orbvaneHits[{id}] ?? 0) + 1)");
        parts.push(match op {
            ">" | ">=" | "<" | "<=" => format!("{count} {op} {n}"),
            "%" => format!("{count} % {n} === 0"),
            _ => format!("{count} === {n}"),
        });
    }
    let condition = parts.join(" && ");
    match bp["logMessage"].as_str().filter(|m| !m.is_empty()) {
        Some(m) => {
            let log = format!("console.log({}), false", log_template(m));
            if condition.is_empty() { log } else { format!("({condition}) && ({log})") }
        }
        None => condition,
    }
}

fn scope_name(scope: &Value) -> String {
    let base = match scope["type"].as_str().unwrap_or("") {
        "local" => "Local",
        "closure" => "Closure",
        "block" => "Block",
        "script" => "Script",
        "global" => "Global",
        "module" => "Module",
        "catch" => "Catch Block",
        "with" => "With Block",
        "eval" => "Eval",
        other => other,
    };
    match scope["name"].as_str().filter(|n| !n.is_empty() && scope["type"] == "closure") {
        Some(n) => format!("{base} ({n})"),
        None => base.to_string(),
    }
}

/// The first line of an exception's description (its message without the stack).
fn exception_text(details: &Value) -> String {
    let description = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("Error");
    description.lines().next().unwrap_or(description).to_string()
}

impl Adapter {
    fn run(&mut self) {
        loop {
            let input = match self.queue.pop_front() {
                Some(i) => i,
                None => match self.inputs.recv() {
                    Ok(i) => i,
                    Err(_) => return,
                },
            };
            match input {
                Input::Dap(msg) if msg["type"] == "request" => {
                    if !self.request(&msg) {
                        return;
                    }
                }
                Input::Dap(_) => {}
                Input::DapClosed => return,
                Input::Cdp(msg) => {
                    if let Some(method) = msg["method"].as_str() {
                        self.cdp_event(method, &msg["params"]);
                    }
                }
                Input::CdpClosed => {
                    self.ws = None;
                    if self.child.is_none() {
                        self.terminate_session(None);
                    }
                }
                Input::Output(category, line) => self.program_output(category, line),
                Input::Exited(code) => self.terminate_session(Some(code.unwrap_or(0))),
            }
        }
    }

    // ------------------------------------------------------------ DAP

    fn send(&mut self, mut msg: Value) {
        self.seq += 1;
        msg["seq"] = json!(self.seq);
        let _ = self.tx.send(msg);
    }

    fn event(&mut self, event: &str, body: Value) {
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }

    fn output(&mut self, category: &str, text: String) {
        self.event("output", json!({ "category": category, "output": text }));
    }

    /// Handles a request; false when the session is over.
    fn request(&mut self, msg: &Value) -> bool {
        let command = msg["command"].as_str().unwrap_or("").to_string();
        let args = &msg["arguments"];
        let result = match command.as_str() {
            "initialize" => Ok(capabilities()),
            "launch" => self.launch(args),
            "attach" => self.attach(args),
            "setBreakpoints" => self.set_breakpoints(args),
            "setExceptionBreakpoints" => self.set_exception_breakpoints(args),
            "configurationDone" => self.configuration_done(),
            "threads" => Ok(json!({ "threads": [{ "id": THREAD_ID, "name": self.thread_name }] })),
            "stackTrace" => Ok(self.stack_trace()),
            "scopes" => self.scopes(args),
            "variables" => self.variables(args),
            "evaluate" => self.evaluate(args),
            "continue" => self.call("Debugger.resume", json!({})).map(|_| json!({ "allThreadsContinued": true })),
            "next" => self.step("Debugger.stepOver"),
            "stepIn" => self.step("Debugger.stepInto"),
            "stepOut" => self.step("Debugger.stepOut"),
            "pause" => {
                self.expect = Some("pause");
                self.call("Debugger.pause", json!({})).map(|_| Value::Null)
            }
            "exceptionInfo" => self.exception_info(),
            "source" => self.source(args),
            "terminate" => {
                self.kill();
                Ok(Value::Null)
            }
            "disconnect" => {
                self.respond(msg, Ok(Value::Null));
                return false;
            }
            _ => Err(format!("Unrecognized request: {command}")),
        };
        let ok = result.is_ok();
        self.respond(msg, result);
        if ok && matches!(command.as_str(), "launch" | "attach") {
            self.event("initialized", json!({}));
        }
        true
    }

    fn respond(&mut self, request: &Value, result: Result<Value, String>) {
        let mut msg = json!({ "type": "response", "request_seq": request["seq"], "command": request["command"], "success": result.is_ok() });
        match result {
            Ok(body) => msg["body"] = body,
            Err(e) => {
                msg["message"] = json!(e);
                msg["body"] = json!({ "error": { "id": 1, "format": e } });
            }
        }
        self.send(msg);
    }

    // ------------------------------------------------------------ CDP

    /// Sends a CDP command and waits for its answer, queueing everything else.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let Some(ws) = &self.ws else { return Err("Not connected to a debuggee.".into()) };
        self.cdp_id += 1;
        let id = self.cdp_id;
        ws.send(&json!({ "id": id, "method": method, "params": params }).to_string()).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.inputs.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Input::Cdp(m)) if m["id"].as_i64() == Some(id) => {
                    return match m.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                        None => Ok(m["result"].clone()),
                    };
                }
                Ok(Input::CdpClosed) => {
                    self.queue.push_back(Input::CdpClosed);
                    return Err("The debuggee disconnected.".into());
                }
                Ok(other) => self.queue.push_back(other),
                Err(_) => return Err(format!("{method} timed out")),
            }
        }
    }

    /// Sends a CDP command without waiting.
    fn notify(&mut self, method: &str, params: Value) {
        if let Some(ws) = &self.ws {
            self.cdp_id += 1;
            let _ = ws.send(&json!({ "id": self.cdp_id, "method": method, "params": params }).to_string());
        }
    }

    fn connect(&mut self, url: &str) -> Result<(), String> {
        let (writer, mut reader) = ws::connect(url).map_err(|e| format!("Couldn't connect to the debuggee at {url}: {e}"))?;
        let tx = self.input_tx.clone();
        // ORBVANE_CDP_LOG=<file> records what the inspector sends.
        let log = std::env::var_os("ORBVANE_CDP_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        thread::spawn(move || {
            let mut log = log;
            while let Some(text) = reader.next() {
                if let Some(f) = &mut log {
                    use std::io::Write;
                    let _ = writeln!(f, "<- {text}");
                }
                if let Ok(msg) = serde_json::from_str(&text) {
                    if tx.send(Input::Cdp(msg)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Input::CdpClosed);
        });
        self.ws = Some(writer);
        self.call("Runtime.enable", json!({}))?;
        self.call("Debugger.enable", json!({}))?;
        if !self.blackbox.is_empty() {
            let _ = self.call("Debugger.setBlackboxPatterns", json!({ "patterns": self.blackbox }));
        }
        if self.source_maps {
            // Each script with a source map pauses before it runs, so breakpoints in its
            // sources can be set first.
            let _ = self.call("Debugger.setInstrumentationBreakpoint", json!({ "instrumentation": "beforeScriptWithSourceMapExecution" }));
        }
        let _ = self.call("Debugger.setAsyncCallStackDepth", json!({ "maxDepth": 32 }));
        Ok(())
    }

    // ------------------------------------------------------------ launch and attach

    fn common_options(&mut self, args: &Value) {
        self.stop_on_entry = args["stopOnEntry"].as_bool().unwrap_or(false);
        self.source_maps = args["sourceMaps"].as_bool().unwrap_or(true);
        self.capture_std = args["outputCapture"] == "std";
        // Negations (`!**/node_modules/mine/**`) aren't supported: those files aren't skipped.
        let globs = args["skipFiles"].as_array().map(|a| a.iter().filter_map(|g| g.as_str()).filter(|g| !g.starts_with('!')).collect::<Vec<_>>()).unwrap_or_default();
        self.blackbox = globs.into_iter().map(blackbox_pattern).collect();
    }

    fn launch(&mut self, args: &Value) -> Result<Value, String> {
        self.common_options(args);
        self.no_debug = args["noDebug"].as_bool().unwrap_or(false);
        let runtime = args["runtimeExecutable"].as_str().unwrap_or("node");
        let program = args["program"].as_str().filter(|p| !p.is_empty());
        if program.is_none() && args["runtimeArgs"].as_array().is_none_or(|a| a.is_empty()) {
            return Err("Attribute 'program' is missing in the launch configuration.".into());
        }
        let cwd = args["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| program.and_then(|p| Path::new(p).parent().map(Path::to_path_buf)))
            .unwrap_or_else(|| PathBuf::from("."));
        let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
        let mut cmd = Command::new(runtime);
        cmd.args(strings(&args["runtimeArgs"]));
        if !self.no_debug {
            cmd.arg("--inspect-brk=127.0.0.1:0");
        }
        if let Some(p) = program {
            cmd.arg(p);
        }
        cmd.args(strings(&args["args"])).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(env) = args["env"].as_object() {
            for (k, v) in env {
                match v.as_str() {
                    Some(v) => cmd.env(k, v),
                    None => cmd.env_remove(k),
                };
            }
        }
        let mut child = cmd.spawn().map_err(|e| format!("Can't launch program '{}': {e}", program.unwrap_or(runtime)))?;
        self.thread_name = program.and_then(|p| Path::new(p).file_name()).map_or("Main Thread".into(), |n| format!("{} [{}]", n.to_string_lossy(), child.id()));
        for (stream, category) in [(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), "stdout"), (child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>), "stderr")] {
            let Some(stream) = stream else { continue };
            let tx = self.input_tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                    if tx.send(Input::Output(category, String::from_utf8_lossy(&line).into_owned())).is_err() {
                        return;
                    }
                    line.clear();
                }
            });
        }
        let child = Arc::new(Mutex::new(child));
        self.child = Some(child.clone());
        let tx = self.input_tx.clone();
        thread::spawn(move || loop {
            let status = child.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
            match status {
                Ok(Some(status)) => {
                    // Let the output threads finish first.
                    thread::sleep(Duration::from_millis(50));
                    let _ = tx.send(Input::Exited(status.code()));
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        });
        if self.no_debug {
            self.capture_std = true;
            return Ok(Value::Null);
        }
        // The inspector says where it listens on stderr.
        let deadline = Instant::now() + TIMEOUT;
        let url = loop {
            match self.inputs.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Input::Output("stderr", line)) if line.contains("Debugger listening on ") => {
                    let url = line.split_whitespace().find(|w| w.starts_with("ws://")).unwrap_or("").to_string();
                    break url;
                }
                Ok(Input::Exited(code)) => {
                    self.queue.push_back(Input::Exited(code));
                    return Err("The program exited before the debugger could attach.".into());
                }
                Ok(other) => self.queue.push_back(other),
                Err(_) => {
                    self.kill();
                    return Err("Timed out waiting for the Node.js inspector.".into());
                }
            }
        };
        self.connect(&url)?;
        Ok(Value::Null)
    }

    fn attach(&mut self, args: &Value) -> Result<Value, String> {
        self.common_options(args);
        let url = match args["websocketAddress"].as_str() {
            Some(u) => u.to_string(),
            None => {
                let address = args["address"].as_str().unwrap_or("127.0.0.1");
                let address = if address == "localhost" { "127.0.0.1" } else { address };
                let host = format!("{address}:{}", args["port"].as_u64().unwrap_or(9229));
                let body = ws::http_get(&host, "/json/list").map_err(|e| format!("Can't connect to the Node.js inspector at {host}: {e}"))?;
                let targets: Value = serde_json::from_str(&body).map_err(|_| format!("Unexpected answer from {host}"))?;
                targets.as_array().and_then(|t| t.iter().find_map(|t| t["webSocketDebuggerUrl"].as_str())).ok_or(format!("No debuggable target at {host}"))?.to_string()
            }
        };
        self.thread_name = "Attached Process".into();
        self.connect(&url)?;
        Ok(Value::Null)
    }

    fn configuration_done(&mut self) -> Result<Value, String> {
        if self.ws.is_some() {
            self.entry_pending = self.child.is_some();
            self.call("Runtime.runIfWaitingForDebugger", json!({}))?;
        }
        Ok(Value::Null)
    }

    fn kill(&mut self) {
        if let Some(child) = &self.child {
            let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
        }
    }

    fn shutdown(&mut self) {
        if let Some(ws) = self.ws.take() {
            ws.close();
        }
        self.kill();
    }

    fn terminate_session(&mut self, exit_code: Option<i32>) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        if let Some(code) = exit_code {
            self.event("exited", json!({ "exitCode": code }));
        }
        self.event("terminated", json!({}));
    }

    fn program_output(&mut self, category: &'static str, line: String) {
        if line.starts_with("Waiting for the debugger to disconnect") {
            // The program is done; it exits once we let go.
            if let Some(ws) = self.ws.take() {
                ws.close();
            }
        }
        if inspector_chatter(&line) || !self.capture_std {
            return;
        }
        self.output(category, line);
    }

    // ------------------------------------------------------------ scripts and breakpoints

    /// The script's source map, loaded on first use.
    fn source_map(&mut self, script_id: &str) -> Option<Arc<SourceMap>> {
        let script = self.scripts.get_mut(script_id)?;
        if script.map.is_none() {
            let map = (|| {
                let url = &script.map_url;
                if url.is_empty() {
                    return None;
                }
                let script_dir = script.path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf);
                let (text, dir) = if let Some(data) = url.strip_prefix("data:") {
                    let (meta, payload) = data.split_once(',')?;
                    let text = if meta.ends_with(";base64") { String::from_utf8_lossy(&crate::base64_decode(payload)).into_owned() } else { crate::percent_decode(payload) };
                    (text, script_dir?)
                } else {
                    let file = match crate::path_of_url(url) {
                        Some(p) => p,
                        None => normalize(&script_dir?.join(url)),
                    };
                    (std::fs::read_to_string(&file).ok()?, file.parent()?.to_path_buf())
                };
                SourceMap::parse(&text, &dir).map(Arc::new)
            })();
            script.map = Some(map);
        }
        script.map.clone().flatten()
    }

    /// Where a generated position is for the user: (DAP source, line, column), 0-based.
    fn locate(&mut self, script_id: &str, line: u32, col: u32) -> (Value, u32, u32) {
        if let Some(map) = self.source_map(script_id) {
            if let Some((path, l, c)) = map.original(line, col) {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return (json!({ "name": name, "path": path }), l, c);
            }
        }
        let Some(script) = self.scripts.get(script_id) else { return (Value::Null, line, col) };
        match &script.path {
            Some(path) => (json!({ "name": path.file_name().map(|n| n.to_string_lossy().into_owned()), "path": path }), line, col),
            None => {
                let url = script.url.clone();
                self.source_refs.push(script_id.to_string());
                (json!({ "name": url, "sourceReference": self.source_refs.len(), "presentationHint": "deemphasize" }), line, col)
            }
        }
    }

    fn set_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let path = normalize(Path::new(args["source"]["path"].as_str().ok_or("No source path")?));
        let wanted: Vec<Bp> = args["breakpoints"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|b| {
                        self.next_bp += 1;
                        Bp {
                            id: self.next_bp,
                            line: (b["line"].as_u64().unwrap_or(1) as u32).saturating_sub(1),
                            column: b["column"].as_u64().map(|c| (c as u32).saturating_sub(1)),
                            condition: breakpoint_condition(self.next_bp, b),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.desired.insert(path.clone(), wanted);
        let results = self.apply_breakpoints(&path);
        Ok(json!({ "breakpoints": results }))
    }

    /// (Re)sets a file's breakpoints in the inspector; their DAP state.
    fn apply_breakpoints(&mut self, path: &Path) -> Vec<Value> {
        for id in self.cdp_bps.remove(path).unwrap_or_default() {
            let _ = self.call("Debugger.removeBreakpoint", json!({ "breakpointId": id }));
        }
        let wanted = self.desired.get(path).cloned().unwrap_or_default();
        if self.ws.is_none() {
            return wanted.iter().map(|b| json!({ "id": b.id, "verified": false, "line": b.line + 1 })).collect();
        }
        let is_js = path.extension().is_some_and(|e| matches!(e.to_str(), Some("js" | "mjs" | "cjs")));
        // Scripts generated from this file.
        let mut mapped = Vec::new();
        if self.source_maps {
            let ids: Vec<String> = self.scripts.iter().filter(|(_, s)| !s.map_url.is_empty()).map(|(id, _)| id.clone()).collect();
            for id in ids {
                if let Some(map) = self.source_map(&id).filter(|m| m.has_source(path)) {
                    mapped.push((self.scripts[&id].url.clone(), map));
                }
            }
        }
        let mut ids = Vec::new();
        let mut results = Vec::new();
        for bp in &wanted {
            let mut verified = false;
            let mut line = bp.line;
            if !mapped.is_empty() {
                for (url, map) in &mapped {
                    let Some((gl, gc)) = map.generated(path, bp.line) else { continue };
                    let params = json!({ "url": url, "lineNumber": gl, "columnNumber": gc, "condition": bp.condition });
                    if let Ok(r) = self.call("Debugger.setBreakpointByUrl", params) {
                        ids.extend(r["breakpointId"].as_str().map(String::from));
                        verified = true;
                    }
                }
            } else if is_js || !self.source_maps {
                let mut params = json!({ "urlRegex": url_regex(path), "lineNumber": bp.line, "condition": bp.condition });
                if let Some(c) = bp.column {
                    params["columnNumber"] = json!(c);
                }
                if let Ok(r) = self.call("Debugger.setBreakpointByUrl", params) {
                    ids.extend(r["breakpointId"].as_str().map(String::from));
                    if let Some(l) = r["locations"][0]["lineNumber"].as_u64() {
                        line = l as u32;
                    }
                    verified = true;
                }
            }
            let mut result = json!({ "id": bp.id, "verified": verified, "line": line + 1 });
            if !verified {
                // Bound when a script generated from this file loads.
                result["message"] = json!("Unbound breakpoint");
            }
            results.push(result);
        }
        self.cdp_bps.insert(path.to_path_buf(), ids);
        results
    }

    fn set_exception_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let filters: Vec<&str> = args["filters"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let state = if filters.contains(&"all") {
            "all"
        } else if filters.contains(&"uncaught") {
            "uncaught"
        } else {
            "none"
        };
        if self.ws.is_some() {
            self.call("Debugger.setPauseOnExceptions", json!({ "state": state }))?;
        }
        Ok(Value::Null)
    }

    // ------------------------------------------------------------ pauses

    fn cdp_event(&mut self, method: &str, params: &Value) {
        match method {
            "Debugger.scriptParsed" => {
                let url = params["url"].as_str().unwrap_or("").to_string();
                let script = Script { path: crate::path_of_url(&url), url, map_url: params["sourceMapURL"].as_str().unwrap_or("").to_string(), map: None };
                self.scripts.insert(params["scriptId"].as_str().unwrap_or("").to_string(), script);
            }
            "Debugger.paused" => self.paused(params),
            "Debugger.resumed" => {
                let was_paused = self.paused.take().is_some();
                self.frames.clear();
                self.handles.clear();
                if was_paused {
                    self.event("continued", json!({ "threadId": THREAD_ID, "allThreadsContinued": true }));
                }
            }
            "Runtime.executionContextCreated" => {
                if self.main_context.is_none() {
                    self.main_context = params["context"]["id"].as_i64();
                }
            }
            "Runtime.executionContextDestroyed" => {
                // The program is done (it waits for us to let go).
                if self.child.is_some() && params["executionContextId"].as_i64() == self.main_context {
                    if let Some(ws) = self.ws.take() {
                        ws.close();
                    }
                }
            }
            "Runtime.consoleAPICalled" if !self.capture_std => {
                let args = params["args"].as_array().cloned().unwrap_or_default();
                let mut text = format::console_message(&args);
                if params["type"] == "trace" {
                    text = format!("Trace: {text}");
                }
                let category = if matches!(params["type"].as_str(), Some("error" | "warning" | "assert" | "trace")) { "stderr" } else { "stdout" };
                self.output(category, text + "\n");
            }
            "Runtime.exceptionThrown" if !self.capture_std => {
                let d = &params["exceptionDetails"];
                let text = d["exception"]["description"].as_str().map(|s| format!("Uncaught {s}")).unwrap_or_else(|| d["text"].as_str().unwrap_or("Uncaught exception").to_string());
                self.output("stderr", text + "\n");
            }
            _ => {}
        }
    }

    fn paused(&mut self, params: &Value) {
        let reason = params["reason"].as_str().unwrap_or("");
        if reason == "instrumentation" {
            // A script with a source map is about to run: bind breakpoints in its sources.
            self.bind_mapped(params["data"]["scriptId"].as_str().unwrap_or(""));
            self.notify("Debugger.resume", json!({}));
            return;
        }
        if self.entry_pending {
            // `--inspect-brk` stops in the main script instead of the instrumentation pause.
            if let Some(script) = params["callFrames"][0]["location"]["scriptId"].as_str() {
                self.bind_mapped(script);
            }
        }
        self.frames = params["callFrames"].as_array().cloned().unwrap_or_default();
        self.handles.clear();
        self.source_refs.clear();
        let hit = params["hitBreakpoints"].as_array().is_some_and(|h| !h.is_empty());
        let exception = matches!(reason, "exception" | "promiseRejection");
        let dap_reason = if std::mem::take(&mut self.entry_pending) && !hit && !exception {
            if !self.stop_on_entry {
                self.frames.clear();
                self.notify("Debugger.resume", json!({}));
                return;
            }
            "entry"
        } else if hit {
            "breakpoint"
        } else if exception {
            "exception"
        } else {
            self.expect.unwrap_or("pause")
        };
        self.expect = None;
        self.paused = Some(params.clone());
        let mut body = json!({ "reason": dap_reason, "threadId": THREAD_ID, "allThreadsStopped": true });
        if exception {
            body["description"] = json!("Paused on exception");
            body["text"] = json!(exception_text(&json!({ "exception": params["data"] })));
        }
        self.event("stopped", body);
    }

    /// Sets the breakpoints of the files script `script_id` was generated from, and tells the
    /// editor they're bound now.
    fn bind_mapped(&mut self, script_id: &str) {
        let Some(map) = self.source_map(script_id) else { return };
        let files: Vec<PathBuf> = map.sources.iter().filter(|s| self.desired.contains_key(*s)).cloned().collect();
        for file in files {
            for bp in self.apply_breakpoints(&file) {
                self.event("breakpoint", json!({ "reason": "changed", "breakpoint": bp }));
            }
        }
    }

    fn step(&mut self, method: &str) -> Result<Value, String> {
        self.expect = Some("step");
        self.call(method, json!({})).map(|_| Value::Null)
    }

    fn stack_trace(&mut self) -> Value {
        let frames = self.frames.clone();
        let mut out = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let loc = &f["location"];
            let script = loc["scriptId"].as_str().unwrap_or("");
            let (source, line, col) = self.locate(script, loc["lineNumber"].as_u64().unwrap_or(0) as u32, loc["columnNumber"].as_u64().unwrap_or(0) as u32);
            let name = f["functionName"].as_str().filter(|n| !n.is_empty()).unwrap_or("<anonymous>");
            let mut frame = json!({ "id": i + 1, "name": name, "line": line + 1, "column": col + 1 });
            if !source.is_null() {
                let internal = source["path"].is_null();
                frame["source"] = source;
                if internal {
                    frame["presentationHint"] = json!("subtle");
                }
            }
            out.push(frame);
        }
        json!({ "stackFrames": out, "totalFrames": frames.len() })
    }

    fn frame(&self, args: &Value) -> Option<&Value> {
        let id = args["frameId"].as_u64()? as usize;
        self.frames.get(id.checked_sub(1)?)
    }

    fn handle(&mut self, h: Handle) -> i64 {
        self.handles.push(h);
        self.handles.len() as i64
    }

    fn scopes(&mut self, args: &Value) -> Result<Value, String> {
        let frame = self.frame(args).cloned().ok_or("Unknown frame")?;
        let mut out = Vec::new();
        for scope in frame["scopeChain"].as_array().cloned().unwrap_or_default() {
            let Some(object) = scope["object"]["objectId"].as_str() else { continue };
            let local = scope["type"] == "local";
            let this = local.then(|| frame["this"].clone()).filter(|t| !t.is_null() && t["type"] != "undefined");
            let reference = self.handle(Handle::Scope { object: object.to_string(), this });
            let mut s = json!({ "name": scope_name(&scope), "variablesReference": reference, "expensive": scope["type"] == "global" });
            if local {
                s["presentationHint"] = json!("locals");
            }
            out.push(s);
        }
        Ok(json!({ "scopes": out }))
    }

    fn variable(&mut self, name: &str, value: &Value) -> Value {
        let reference = value["objectId"].as_str().map_or(0, |id| self.handle(Handle::Object(id.to_string())));
        let ty = value["className"].as_str().or(value["type"].as_str()).unwrap_or("");
        json!({ "name": name, "value": format::describe(value), "type": ty, "variablesReference": reference })
    }

    fn variables(&mut self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
        let (object, this) = match self.handles.get(reference.wrapping_sub(1)) {
            Some(Handle::Scope { object, this }) => (object.clone(), this.clone()),
            Some(Handle::Object(object)) => (object.clone(), None),
            None => return Err("Unknown variables reference".into()),
        };
        let r = self.call("Runtime.getProperties", json!({ "objectId": object, "ownProperties": true, "generatePreview": true }))?;
        let mut out = Vec::new();
        if let Some(this) = this {
            out.push(self.variable("this", &this));
        }
        for p in r["result"].as_array().cloned().unwrap_or_default() {
            let name = p["name"].as_str().unwrap_or("");
            match p.get("value") {
                Some(v) => out.push(self.variable(name, v)),
                // A getter: not run just to show it.
                None if p["get"]["type"] == "function" => out.push(json!({ "name": name, "value": "(...)", "variablesReference": 0 })),
                None => {}
            }
        }
        for p in r["internalProperties"].as_array().cloned().unwrap_or_default() {
            if let Some(v) = p.get("value") {
                out.push(self.variable(p["name"].as_str().unwrap_or(""), v));
            }
        }
        Ok(json!({ "variables": out }))
    }

    fn evaluate(&mut self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("");
        let context = args["context"].as_str().unwrap_or("repl");
        let r = match self.frame(args).map(|f| f["callFrameId"].clone()) {
            Some(frame) => self.call(
                "Debugger.evaluateOnCallFrame",
                json!({
                    "callFrameId": frame, "expression": expression, "generatePreview": true,
                    "includeCommandLineAPI": context == "repl", "silent": context != "repl", "throwOnSideEffect": context == "hover",
                }),
            )?,
            None => self.call(
                "Runtime.evaluate",
                json!({ "expression": expression, "generatePreview": true, "includeCommandLineAPI": true, "replMode": context == "repl", "silent": context != "repl" }),
            )?,
        };
        if let Some(details) = r.get("exceptionDetails") {
            return Err(exception_text(details));
        }
        let v = self.variable("", &r["result"]);
        Ok(json!({ "result": v["value"], "type": v["type"], "variablesReference": v["variablesReference"] }))
    }

    fn exception_info(&mut self) -> Result<Value, String> {
        let paused = self.paused.as_ref().ok_or("Not paused")?;
        let data = &paused["data"];
        let description = data["description"].as_str().unwrap_or("");
        Ok(json!({
            "exceptionId": data["className"].as_str().unwrap_or("Error"),
            "description": description.lines().next().unwrap_or(""),
            "breakMode": if paused["data"]["uncaught"].as_bool().unwrap_or(false) { "unhandled" } else { "always" },
            "details": { "message": description.lines().next().unwrap_or(""), "stackTrace": description },
        }))
    }

    fn source(&mut self, args: &Value) -> Result<Value, String> {
        let reference = args["sourceReference"].as_u64().unwrap_or(0) as usize;
        let script = self.source_refs.get(reference.wrapping_sub(1)).cloned().ok_or("Unknown source")?;
        let r = self.call("Debugger.getScriptSource", json!({ "scriptId": script }))?;
        Ok(json!({ "content": r["scriptSource"], "mimeType": "text/javascript" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_conditions_and_patterns() {
        assert_eq!(log_template("x is {x}, `y` {a[{b}]}"), "`x is ${x}, \\`y\\` ${a[{b}]}`");
        assert_eq!(breakpoint_condition(3, &json!({ "logMessage": "hi {n}" })), "console.log(`hi ${n}`), false");
        let c = breakpoint_condition(4, &json!({ "condition": "n > 2", "hitCondition": ">= 3" }));
        assert!(c.starts_with("(n > 2) && (") && c.ends_with(">= 3"), "{c}");
        assert_eq!(url_regex(Path::new("/a b/x.js")), "^(?:file://)?(?:/a b/x\\.js|/a%20b/x\\.js)$");
        assert_eq!(blackbox_pattern("<node_internals>/**"), "^node:|^internal/");
        assert_eq!(blackbox_pattern("/p/node_modules/**/*.js"), "^(?:file://)?/p/node_modules/.*/[^/]*\\.js$");
        assert!(inspector_chatter("Debugger listening on ws://127.0.0.1:1/x"));
    }
}
