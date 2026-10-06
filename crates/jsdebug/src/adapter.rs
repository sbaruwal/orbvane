//! The debug adapter: DAP requests from the editor become inspector (CDP) calls, inspector
//! events become DAP events. One thread handles everything in order; a CDP call waits for its
//! answer and queues whatever else arrives meanwhile.
//!
//! Each JavaScript thread is a target with its own inspector session and a DAP thread id: the
//! launched (or attached) process is thread 1; child processes it starts (when they run Node.js)
//! and worker threads are added as they appear. Child processes find us through a preload script
//! (`NODE_OPTIONS=--require`, `BOOT`): it opens their inspector, writes its address into a folder
//! we watch and waits until we've attached. Workers come through their process's session
//! (`NodeWorker` domain, messages wrapped in `NodeWorker.sendMessageToWorker`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// The launched or attached process's thread.
const MAIN: i64 = 1;
/// Frame ids: thread `t`'s frame `i` is `(t - 1) * FRAMES + i + 1`.
const FRAMES: i64 = 100_000;

/// Run in every Node.js process the program starts (not the program itself, whose inspector is
/// already open, and not in workers): opens the inspector, tells the adapter where it listens
/// (a file in `ORBVANE_JSDEBUG_DIR`, written whole then renamed) and waits for it to attach.
const BOOT: &str = r#"// Lets the Orbvane debugger attach to this Node.js process before it runs.
(() => {
  const dir = process.env.ORBVANE_JSDEBUG_DIR;
  let inspector, isMain;
  try {
    inspector = require('inspector');
    isMain = require('worker_threads').isMainThread;
  } catch {
    return;
  }
  if (!dir || !isMain || inspector.url()) return;
  const fs = require('fs'), path = require('path');
  try {
    inspector.open(0, '127.0.0.1', false);
    const title = path.basename(process.argv[1] || process.argv0 || 'node');
    const file = path.join(dir, process.pid + '.json');
    fs.writeFileSync(file + '.tmp', JSON.stringify({ url: inspector.url(), pid: process.pid, title }));
    fs.renameSync(file + '.tmp', file);
  } catch {
    // The debugging session is over: run without it.
    try { inspector.close(); } catch {}
    return;
  }
  inspector.waitForDebugger();
})();
"#;

enum Input {
    Dap(Value),
    DapClosed,
    /// A message from target `t`'s inspector.
    Cdp(i64, Value),
    CdpClosed(i64),
    /// A line the program wrote ("stdout" or "stderr").
    Output(&'static str, String),
    Exited(Option<i32>),
    /// A child process waiting for us: its inspector's address, pid and script name.
    Child { url: String, pid: u64, title: String },
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

/// How a target's inspector is reached.
enum Conn {
    Ws(ws::Writer),
    /// A worker: through its parent's session.
    Worker { parent: i64, session: String },
}

/// A JavaScript thread being debugged (a DAP thread).
struct Target {
    name: String,
    conn: Option<Conn>,
    scripts: HashMap<String, Script>,
    /// The inspector's breakpoint ids for each file's breakpoints.
    cdp_bps: HashMap<PathBuf, Vec<String>>,
    /// The call frames of the current pause.
    frames: Vec<Value>,
    paused: Option<Value>,
    /// The reason to report for the next pause (a step, a pause request).
    expect: Option<&'static str>,
    /// The program's first pause (`--inspect-brk`) is still to come.
    entry_pending: bool,
    main_context: Option<i64>,
    /// A process (not a worker) that keeps running until we let go once its code is done.
    process: bool,
}

impl Target {
    fn new(name: String, conn: Conn, process: bool) -> Target {
        Target {
            name,
            conn: Some(conn),
            scripts: HashMap::new(),
            cdp_bps: HashMap::new(),
            frames: Vec::new(),
            paused: None,
            expect: None,
            entry_pending: false,
            main_context: None,
            process,
        }
    }
}

struct Adapter {
    tx: Sender<Value>,
    seq: i64,
    inputs: Receiver<Input>,
    input_tx: Sender<Input>,
    /// Inputs that arrived while a CDP call waited.
    queue: VecDeque<Input>,
    cdp_id: i64,
    targets: BTreeMap<i64, Target>,
    next_thread: i64,
    /// Workers' sessions, to their targets.
    workers: HashMap<String, i64>,
    /// The program we launched (None when attached).
    child: Option<Arc<Mutex<Child>>>,
    /// Where child processes say they're waiting, and the flag that stops watching it.
    child_dir: Option<(PathBuf, Arc<AtomicBool>)>,
    auto_attach: bool,
    no_debug: bool,
    stop_on_entry: bool,
    source_maps: bool,
    /// `"outputCapture": "std"`: the program's stdout/stderr instead of console calls.
    capture_std: bool,
    blackbox: Vec<String>,
    /// Pause on exceptions: "none", "uncaught" or "all".
    exceptions: &'static str,
    /// Breakpoints the editor wants, by file.
    desired: HashMap<PathBuf, Vec<Bp>>,
    next_bp: i64,
    /// `variablesReference`s and `sourceReference`s, with their targets.
    handles: HashMap<i64, (i64, Handle)>,
    source_refs: HashMap<i64, (i64, String)>,
    next_ref: i64,
    /// The thread that stopped last (evaluations without a frame go there).
    last_stopped: i64,
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
        cdp_id: 0,
        targets: BTreeMap::new(),
        next_thread: MAIN,
        workers: HashMap::new(),
        child: None,
        child_dir: None,
        auto_attach: true,
        no_debug: false,
        stop_on_entry: false,
        source_maps: true,
        capture_std: false,
        blackbox: Vec::new(),
        exceptions: "none",
        desired: HashMap::new(),
        next_bp: 0,
        handles: HashMap::new(),
        source_refs: HashMap::new(),
        next_ref: 0,
        last_stopped: MAIN,
        terminated: false,
    };
    a.run();
    a.shutdown();
}

/// Watches `dir` for child processes' addresses until `stop` is set.
fn watch_children(dir: PathBuf, stop: Arc<AtomicBool>, tx: Sender<Input>) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let info: Option<Value> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
                let _ = std::fs::remove_file(&path);
                let Some(info) = info else { continue };
                let child = Input::Child {
                    url: info["url"].as_str().unwrap_or("").to_string(),
                    pid: info["pid"].as_u64().unwrap_or(0),
                    title: info["title"].as_str().unwrap_or("node").to_string(),
                };
                if tx.send(child).is_err() {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(30));
        }
    });
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
                    Ok(i) => self.translate(i),
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
                Input::Cdp(t, msg) => {
                    if let Some(method) = msg["method"].as_str() {
                        self.cdp_event(t, method, &msg["params"]);
                    }
                }
                Input::CdpClosed(t) => self.target_gone(t),
                Input::Output(category, line) => self.program_output(category, line),
                Input::Exited(code) => self.terminate_session(Some(code.unwrap_or(0))),
                Input::Child { url, pid, title } => self.attach_child(&url, pid, &title),
            }
        }
    }

    /// Unwraps workers' messages, which arrive as events of their parent's session.
    fn translate(&self, mut input: Input) -> Input {
        loop {
            let Input::Cdp(_, msg) = &input else { return input };
            let worker = |m: &Value| m["params"]["sessionId"].as_str().and_then(|s| self.workers.get(s)).copied();
            input = match msg["method"].as_str() {
                Some("NodeWorker.receivedMessageFromWorker") => {
                    let Some(w) = worker(msg) else { return input };
                    match msg["params"]["message"].as_str().and_then(|m| serde_json::from_str(m).ok()) {
                        Some(inner) => Input::Cdp(w, inner),
                        None => return input,
                    }
                }
                Some("NodeWorker.detachedFromWorker") => match worker(msg) {
                    Some(w) => Input::CdpClosed(w),
                    None => return input,
                },
                _ => return input,
            };
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

    fn thread_of(args: &Value) -> i64 {
        args["threadId"].as_i64().unwrap_or(MAIN)
    }

    /// Handles a request; false when the session is over.
    fn request(&mut self, msg: &Value) -> bool {
        let command = msg["command"].as_str().unwrap_or("").to_string();
        let args = &msg["arguments"];
        let t = Self::thread_of(args);
        let result = match command.as_str() {
            "initialize" => Ok(capabilities()),
            "launch" => self.launch(args),
            "attach" => self.attach(args),
            "setBreakpoints" => self.set_breakpoints(args),
            "setExceptionBreakpoints" => self.set_exception_breakpoints(args),
            "configurationDone" => self.configuration_done(),
            "threads" => {
                let threads: Vec<Value> = self.targets.iter().map(|(id, t)| json!({ "id": id, "name": t.name })).collect();
                Ok(json!({ "threads": threads }))
            }
            "stackTrace" => Ok(self.stack_trace(t)),
            "scopes" => self.scopes(args),
            "variables" => self.variables(args),
            "evaluate" => self.evaluate(args),
            "continue" => self.call(t, "Debugger.resume", json!({})).map(|_| json!({ "allThreadsContinued": self.targets.len() == 1 })),
            "next" => self.step(t, "Debugger.stepOver"),
            "stepIn" => self.step(t, "Debugger.stepInto"),
            "stepOut" => self.step(t, "Debugger.stepOut"),
            "pause" => {
                if let Some(target) = self.targets.get_mut(&t) {
                    target.expect = Some("pause");
                }
                self.call(t, "Debugger.pause", json!({})).map(|_| Value::Null)
            }
            "exceptionInfo" => self.exception_info(t),
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

    /// Sends a message to target `t`'s inspector (a worker's through its parent).
    fn send_to(&mut self, t: i64, msg: Value) -> Result<(), String> {
        match self.targets.get(&t).and_then(|t| t.conn.as_ref()) {
            Some(Conn::Ws(w)) => w.send(&msg.to_string()).map_err(|e| e.to_string()),
            Some(Conn::Worker { parent, session }) => {
                let (parent, session) = (*parent, session.clone());
                self.cdp_id += 1;
                let wrapped = json!({ "id": self.cdp_id, "method": "NodeWorker.sendMessageToWorker", "params": { "sessionId": session, "message": msg.to_string() } });
                self.send_to(parent, wrapped)
            }
            None => Err("Not connected to a debuggee.".into()),
        }
    }

    /// Sends a CDP command to target `t` and waits for its answer, queueing everything else.
    fn call(&mut self, t: i64, method: &str, params: Value) -> Result<Value, String> {
        self.cdp_id += 1;
        let id = self.cdp_id;
        self.send_to(t, json!({ "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let input = match self.inputs.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(i) => self.translate(i),
                Err(_) => return Err(format!("{method} timed out")),
            };
            match input {
                Input::Cdp(from, m) if from == t && m["id"].as_i64() == Some(id) => {
                    return match m.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                        None => Ok(m["result"].clone()),
                    };
                }
                Input::CdpClosed(from) if from == t => {
                    self.queue.push_back(Input::CdpClosed(from));
                    return Err("The debuggee disconnected.".into());
                }
                other => self.queue.push_back(other),
            }
        }
    }

    /// Sends a CDP command without waiting.
    fn notify(&mut self, t: i64, method: &str, params: Value) {
        self.cdp_id += 1;
        let _ = self.send_to(t, json!({ "id": self.cdp_id, "method": method, "params": params }));
    }

    /// Connects to the inspector at `url` as a new thread; its id.
    fn connect(&mut self, url: &str, name: String) -> Result<i64, String> {
        let (writer, mut reader) = ws::connect(url).map_err(|e| format!("Couldn't connect to the debuggee at {url}: {e}"))?;
        let t = self.next_thread;
        self.next_thread += 1;
        let tx = self.input_tx.clone();
        // ORBVANE_CDP_LOG=<file> records what the inspector sends.
        let log = std::env::var_os("ORBVANE_CDP_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        thread::spawn(move || {
            let mut log = log;
            while let Some(text) = reader.next() {
                if let Some(f) = &mut log {
                    use std::io::Write;
                    let _ = writeln!(f, "<- [{t}] {text}");
                }
                if let Ok(msg) = serde_json::from_str(&text) {
                    if tx.send(Input::Cdp(t, msg)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Input::CdpClosed(t));
        });
        self.targets.insert(t, Target::new(name, Conn::Ws(writer), true));
        Ok(t)
    }

    /// Readies a new target: domains, options and the breakpoints set so far.
    fn setup(&mut self, t: i64) -> Result<(), String> {
        self.call(t, "Runtime.enable", json!({}))?;
        self.call(t, "Debugger.enable", json!({}))?;
        if !self.blackbox.is_empty() {
            let _ = self.call(t, "Debugger.setBlackboxPatterns", json!({ "patterns": self.blackbox }));
        }
        if self.source_maps {
            // Each script with a source map pauses before it runs, so breakpoints in its
            // sources can be set first.
            let _ = self.call(t, "Debugger.setInstrumentationBreakpoint", json!({ "instrumentation": "beforeScriptWithSourceMapExecution" }));
        }
        let _ = self.call(t, "Debugger.setAsyncCallStackDepth", json!({ "maxDepth": 32 }));
        if self.exceptions != "none" {
            let _ = self.call(t, "Debugger.setPauseOnExceptions", json!({ "state": self.exceptions }));
        }
        let files: Vec<PathBuf> = self.desired.keys().cloned().collect();
        for file in files {
            self.apply_breakpoints(t, &file);
        }
        if self.auto_attach {
            let _ = self.call(t, "NodeWorker.enable", json!({ "waitForDebuggerOnStart": true }));
        }
        Ok(())
    }

    /// A target that waits for us (a child process or worker) is ready: it runs.
    fn start_thread(&mut self, t: i64) {
        self.event("thread", json!({ "reason": "started", "threadId": t }));
        if let Err(e) = self.setup(t) {
            self.output("console", format!("Couldn't debug {}: {e}\n", self.targets.get(&t).map_or("a thread", |t| t.name.as_str())));
        }
        self.notify(t, "Runtime.runIfWaitingForDebugger", json!({}));
    }

    fn attach_child(&mut self, url: &str, pid: u64, title: &str) {
        match self.connect(url, format!("{title} [{pid}]")) {
            Ok(t) => self.start_thread(t),
            Err(e) => self.output("console", format!("{e}\n")),
        }
    }

    /// Target `t`'s inspector went away (and the workers it had).
    fn target_gone(&mut self, t: i64) {
        if t == MAIN {
            if let Some(target) = self.targets.get_mut(&MAIN) {
                target.conn = None;
            }
            if self.child.is_none() {
                self.terminate_session(None);
            }
            return;
        }
        if self.targets.remove(&t).is_none() {
            return;
        }
        self.workers.retain(|_, w| *w != t);
        self.handles.retain(|_, (owner, _)| *owner != t);
        self.event("thread", json!({ "reason": "exited", "threadId": t }));
        let orphans: Vec<i64> = self.targets.iter().filter(|(_, x)| matches!(x.conn, Some(Conn::Worker { parent, .. }) if parent == t)).map(|(id, _)| *id).collect();
        for w in orphans {
            self.target_gone(w);
        }
    }

    // ------------------------------------------------------------ launch and attach

    fn common_options(&mut self, args: &Value) {
        self.stop_on_entry = args["stopOnEntry"].as_bool().unwrap_or(false);
        self.source_maps = args["sourceMaps"].as_bool().unwrap_or(true);
        self.capture_std = args["outputCapture"] == "std";
        self.auto_attach = args["autoAttachChildProcesses"].as_bool().unwrap_or(true);
        // Negations (`!**/node_modules/mine/**`) aren't supported: those files aren't skipped.
        let globs = args["skipFiles"].as_array().map(|a| a.iter().filter_map(|g| g.as_str()).filter(|g| !g.starts_with('!')).collect::<Vec<_>>()).unwrap_or_default();
        self.blackbox = globs.into_iter().map(blackbox_pattern).collect();
    }

    /// Makes Node.js processes the program starts report to us: the preload script in a
    /// fresh folder, watched for their addresses. Returns the `NODE_OPTIONS` to add.
    fn watch_child_processes(&mut self) -> Option<String> {
        static SESSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SESSIONS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orbvane-jsdebug-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let boot = dir.join("boot.js");
        std::fs::write(&boot, BOOT).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_children(dir.clone(), stop.clone(), self.input_tx.clone());
        self.child_dir = Some((dir, stop));
        Some(format!("--require \"{}\"", boot.display()))
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
        if !self.no_debug && self.auto_attach {
            if let Some(require) = self.watch_child_processes() {
                let theirs = args["env"]["NODE_OPTIONS"].as_str().map(String::from).or_else(|| std::env::var("NODE_OPTIONS").ok()).unwrap_or_default();
                cmd.env("NODE_OPTIONS", format!("{theirs} {require}").trim());
                cmd.env("ORBVANE_JSDEBUG_DIR", &self.child_dir.as_ref().unwrap().0);
            }
        }
        let mut child = cmd.spawn().map_err(|e| format!("Can't launch program '{}': {e}", program.unwrap_or(runtime)))?;
        let name = program.and_then(|p| Path::new(p).file_name()).map_or("Main Thread".into(), |n| format!("{} [{}]", n.to_string_lossy(), child.id()));
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
        let t = self.connect(&url, name)?;
        self.setup(t)?;
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
        let t = self.connect(&url, "Attached Process".into())?;
        self.setup(t)?;
        Ok(Value::Null)
    }

    fn configuration_done(&mut self) -> Result<Value, String> {
        if self.targets.contains_key(&MAIN) {
            let launched = self.child.is_some();
            if let Some(main) = self.targets.get_mut(&MAIN) {
                main.entry_pending = launched;
            }
            self.call(MAIN, "Runtime.runIfWaitingForDebugger", json!({}))?;
        }
        Ok(Value::Null)
    }

    fn kill(&mut self) {
        if let Some(child) = &self.child {
            let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
        }
    }

    fn close_all(&mut self) {
        for t in self.targets.values_mut() {
            if let Some(Conn::Ws(w)) = t.conn.take() {
                w.close();
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some((dir, stop)) = self.child_dir.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_dir_all(dir);
        }
        self.close_all();
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
        // The program is done; it exits once we let go. (Child processes print this to the
        // same stream: then the end of the program's main context says it instead.)
        if line.starts_with("Waiting for the debugger to disconnect") && self.child_dir.is_none() {
            if let Some(Conn::Ws(w)) = self.targets.get_mut(&MAIN).and_then(|t| t.conn.take()) {
                w.close();
            }
        }
        if inspector_chatter(&line) || !self.capture_std {
            return;
        }
        self.output(category, line);
    }

    // ------------------------------------------------------------ scripts and breakpoints

    /// A script's source map, loaded on first use.
    fn source_map(&mut self, t: i64, script_id: &str) -> Option<Arc<SourceMap>> {
        let script = self.targets.get_mut(&t)?.scripts.get_mut(script_id)?;
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
    fn locate(&mut self, t: i64, script_id: &str, line: u32, col: u32) -> (Value, u32, u32) {
        if let Some(map) = self.source_map(t, script_id) {
            if let Some((path, l, c)) = map.original(line, col) {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return (json!({ "name": name, "path": path }), l, c);
            }
        }
        let Some(script) = self.targets.get(&t).and_then(|x| x.scripts.get(script_id)) else { return (Value::Null, line, col) };
        match &script.path {
            Some(path) => (json!({ "name": path.file_name().map(|n| n.to_string_lossy().into_owned()), "path": path }), line, col),
            None => {
                let url = script.url.clone();
                self.next_ref += 1;
                self.source_refs.insert(self.next_ref, (t, script_id.to_string()));
                (json!({ "name": url, "sourceReference": self.next_ref, "presentationHint": "deemphasize" }), line, col)
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
        // Every thread gets them; one that binds them makes them verified.
        let threads: Vec<i64> = self.targets.keys().copied().collect();
        let mut results = self.unbound(&path);
        for t in threads {
            for (r, new) in results.iter_mut().zip(self.apply_breakpoints(t, &path)) {
                if new["verified"] == true && r["verified"] != true {
                    *r = new;
                }
            }
        }
        Ok(json!({ "breakpoints": results }))
    }

    /// A file's breakpoints as not bound yet.
    fn unbound(&self, path: &Path) -> Vec<Value> {
        let wanted = self.desired.get(path).cloned().unwrap_or_default();
        wanted.iter().map(|b| json!({ "id": b.id, "verified": false, "line": b.line + 1, "message": "Unbound breakpoint" })).collect()
    }

    /// (Re)sets a file's breakpoints in target `t`; their DAP state.
    fn apply_breakpoints(&mut self, t: i64, path: &Path) -> Vec<Value> {
        let old = self.targets.get_mut(&t).and_then(|x| x.cdp_bps.remove(path)).unwrap_or_default();
        for id in old {
            let _ = self.call(t, "Debugger.removeBreakpoint", json!({ "breakpointId": id }));
        }
        let wanted = self.desired.get(path).cloned().unwrap_or_default();
        if self.targets.get(&t).is_none_or(|x| x.conn.is_none()) {
            return self.unbound(path);
        }
        let is_js = path.extension().is_some_and(|e| matches!(e.to_str(), Some("js" | "mjs" | "cjs")));
        // Scripts generated from this file.
        let mut mapped = Vec::new();
        if self.source_maps {
            let ids: Vec<String> = self.targets[&t].scripts.iter().filter(|(_, s)| !s.map_url.is_empty()).map(|(id, _)| id.clone()).collect();
            for id in ids {
                if let Some(map) = self.source_map(t, &id).filter(|m| m.has_source(path)) {
                    mapped.push((self.targets[&t].scripts[&id].url.clone(), map));
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
                    if let Ok(r) = self.call(t, "Debugger.setBreakpointByUrl", params) {
                        ids.extend(r["breakpointId"].as_str().map(String::from));
                        verified = true;
                    }
                }
            } else if is_js || !self.source_maps {
                let mut params = json!({ "urlRegex": url_regex(path), "lineNumber": bp.line, "condition": bp.condition });
                if let Some(c) = bp.column {
                    params["columnNumber"] = json!(c);
                }
                if let Ok(r) = self.call(t, "Debugger.setBreakpointByUrl", params) {
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
        if let Some(x) = self.targets.get_mut(&t) {
            x.cdp_bps.insert(path.to_path_buf(), ids);
        }
        results
    }

    fn set_exception_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let filters: Vec<&str> = args["filters"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        self.exceptions = if filters.contains(&"all") {
            "all"
        } else if filters.contains(&"uncaught") {
            "uncaught"
        } else {
            "none"
        };
        let threads: Vec<i64> = self.targets.keys().copied().collect();
        for t in threads {
            self.call(t, "Debugger.setPauseOnExceptions", json!({ "state": self.exceptions }))?;
        }
        Ok(Value::Null)
    }

    // ------------------------------------------------------------ pauses

    fn cdp_event(&mut self, t: i64, method: &str, params: &Value) {
        match method {
            "Debugger.scriptParsed" => {
                let url = params["url"].as_str().unwrap_or("").to_string();
                let script = Script { path: crate::path_of_url(&url), url, map_url: params["sourceMapURL"].as_str().unwrap_or("").to_string(), map: None };
                if let Some(x) = self.targets.get_mut(&t) {
                    x.scripts.insert(params["scriptId"].as_str().unwrap_or("").to_string(), script);
                }
            }
            "Debugger.paused" => self.paused(t, params),
            "Debugger.resumed" => {
                let Some(x) = self.targets.get_mut(&t) else { return };
                let was_paused = x.paused.take().is_some();
                x.frames.clear();
                self.handles.retain(|_, (owner, _)| *owner != t);
                if was_paused {
                    self.event("continued", json!({ "threadId": t, "allThreadsContinued": self.targets.len() == 1 }));
                }
            }
            "Runtime.executionContextCreated" => {
                if let Some(x) = self.targets.get_mut(&t).filter(|x| x.main_context.is_none()) {
                    x.main_context = params["context"]["id"].as_i64();
                }
            }
            "Runtime.executionContextDestroyed" => {
                // A process is done (it waits for us to let go).
                let launched = t != MAIN || self.child.is_some();
                if let Some(x) = self.targets.get_mut(&t).filter(|x| x.process && launched && params["executionContextId"].as_i64() == x.main_context) {
                    if let Some(Conn::Ws(w)) = x.conn.take() {
                        w.close();
                    }
                }
            }
            "NodeWorker.attachedToWorker" => {
                let Some(session) = params["sessionId"].as_str().map(String::from) else { return };
                let info = &params["workerInfo"];
                let title = info["title"].as_str().filter(|s| !s.is_empty()).or(info["url"].as_str()).unwrap_or("worker");
                let title = title.rsplit('/').next().unwrap_or(title);
                let w = self.next_thread;
                self.next_thread += 1;
                let name = format!("Worker {} ({title})", info["workerId"].as_str().unwrap_or(""));
                let mut target = Target::new(name, Conn::Worker { parent: t, session: session.clone() }, false);
                // A worker that waited for us stops at its first line, like `--inspect-brk`.
                target.entry_pending = params["waitingForDebugger"].as_bool().unwrap_or(true);
                self.targets.insert(w, target);
                self.workers.insert(session, w);
                self.start_thread(w);
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

    fn paused(&mut self, t: i64, params: &Value) {
        let reason = params["reason"].as_str().unwrap_or("");
        if reason == "instrumentation" {
            // A script with a source map is about to run: bind breakpoints in its sources.
            self.bind_mapped(t, params["data"]["scriptId"].as_str().unwrap_or(""));
            self.notify(t, "Debugger.resume", json!({}));
            return;
        }
        let Some(entry_pending) = self.targets.get(&t).map(|x| x.entry_pending) else { return };
        if entry_pending {
            // `--inspect-brk` stops in the main script instead of the instrumentation pause.
            if let Some(script) = params["callFrames"][0]["location"]["scriptId"].as_str() {
                self.bind_mapped(t, script);
            }
        }
        self.handles.retain(|_, (owner, _)| *owner != t);
        self.source_refs.retain(|_, (owner, _)| *owner != t);
        let stop_on_entry = self.stop_on_entry;
        let x = self.targets.get_mut(&t).unwrap();
        x.frames = params["callFrames"].as_array().cloned().unwrap_or_default();
        let hit = params["hitBreakpoints"].as_array().is_some_and(|h| !h.is_empty());
        let exception = matches!(reason, "exception" | "promiseRejection");
        let dap_reason = if std::mem::take(&mut x.entry_pending) && !hit && !exception {
            if !stop_on_entry {
                x.frames.clear();
                self.notify(t, "Debugger.resume", json!({}));
                return;
            }
            "entry"
        } else if hit {
            "breakpoint"
        } else if exception {
            "exception"
        } else {
            x.expect.unwrap_or("pause")
        };
        x.expect = None;
        x.paused = Some(params.clone());
        self.last_stopped = t;
        let mut body = json!({ "reason": dap_reason, "threadId": t, "allThreadsStopped": self.targets.len() == 1 });
        if exception {
            body["description"] = json!("Paused on exception");
            body["text"] = json!(exception_text(&json!({ "exception": params["data"] })));
        }
        self.event("stopped", body);
    }

    /// Sets the breakpoints of the files script `script_id` (of target `t`) was generated
    /// from, and tells the editor they're bound now.
    fn bind_mapped(&mut self, t: i64, script_id: &str) {
        let Some(map) = self.source_map(t, script_id) else { return };
        let files: Vec<PathBuf> = map.sources.iter().filter(|s| self.desired.contains_key(*s)).cloned().collect();
        for file in files {
            for bp in self.apply_breakpoints(t, &file) {
                self.event("breakpoint", json!({ "reason": "changed", "breakpoint": bp }));
            }
        }
    }

    fn step(&mut self, t: i64, method: &str) -> Result<Value, String> {
        if let Some(x) = self.targets.get_mut(&t) {
            x.expect = Some("step");
        }
        self.call(t, method, json!({})).map(|_| Value::Null)
    }

    fn stack_trace(&mut self, t: i64) -> Value {
        let frames = self.targets.get(&t).map(|x| x.frames.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let loc = &f["location"];
            let script = loc["scriptId"].as_str().unwrap_or("");
            let (source, line, col) = self.locate(t, script, loc["lineNumber"].as_u64().unwrap_or(0) as u32, loc["columnNumber"].as_u64().unwrap_or(0) as u32);
            let name = f["functionName"].as_str().filter(|n| !n.is_empty()).unwrap_or("<anonymous>");
            let mut frame = json!({ "id": (t - 1) * FRAMES + i as i64 + 1, "name": name, "line": line + 1, "column": col + 1 });
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

    /// The frame `frameId` names: (its thread, the frame).
    fn frame(&self, args: &Value) -> Option<(i64, &Value)> {
        let id = args["frameId"].as_i64()?.checked_sub(1)?;
        let t = id / FRAMES + 1;
        Some((t, self.targets.get(&t)?.frames.get((id % FRAMES) as usize)?))
    }

    fn handle(&mut self, t: i64, h: Handle) -> i64 {
        self.next_ref += 1;
        self.handles.insert(self.next_ref, (t, h));
        self.next_ref
    }

    fn scopes(&mut self, args: &Value) -> Result<Value, String> {
        let (t, frame) = self.frame(args).map(|(t, f)| (t, f.clone())).ok_or("Unknown frame")?;
        let mut out = Vec::new();
        for scope in frame["scopeChain"].as_array().cloned().unwrap_or_default() {
            let Some(object) = scope["object"]["objectId"].as_str() else { continue };
            let local = scope["type"] == "local";
            let this = local.then(|| frame["this"].clone()).filter(|t| !t.is_null() && t["type"] != "undefined");
            let reference = self.handle(t, Handle::Scope { object: object.to_string(), this });
            let mut s = json!({ "name": scope_name(&scope), "variablesReference": reference, "expensive": scope["type"] == "global" });
            if local {
                s["presentationHint"] = json!("locals");
            }
            out.push(s);
        }
        Ok(json!({ "scopes": out }))
    }

    fn variable(&mut self, t: i64, name: &str, value: &Value) -> Value {
        let reference = value["objectId"].as_str().map_or(0, |id| self.handle(t, Handle::Object(id.to_string())));
        let ty = value["className"].as_str().or(value["type"].as_str()).unwrap_or("");
        json!({ "name": name, "value": format::describe(value), "type": ty, "variablesReference": reference })
    }

    fn variables(&mut self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_i64().unwrap_or(0);
        let (t, object, this) = match self.handles.get(&reference) {
            Some((t, Handle::Scope { object, this })) => (*t, object.clone(), this.clone()),
            Some((t, Handle::Object(object))) => (*t, object.clone(), None),
            None => return Err("Unknown variables reference".into()),
        };
        let r = self.call(t, "Runtime.getProperties", json!({ "objectId": object, "ownProperties": true, "generatePreview": true }))?;
        let mut out = Vec::new();
        if let Some(this) = this {
            out.push(self.variable(t, "this", &this));
        }
        for p in r["result"].as_array().cloned().unwrap_or_default() {
            let name = p["name"].as_str().unwrap_or("");
            match p.get("value") {
                Some(v) => out.push(self.variable(t, name, v)),
                // A getter: not run just to show it.
                None if p["get"]["type"] == "function" => out.push(json!({ "name": name, "value": "(...)", "variablesReference": 0 })),
                None => {}
            }
        }
        for p in r["internalProperties"].as_array().cloned().unwrap_or_default() {
            if let Some(v) = p.get("value") {
                out.push(self.variable(t, p["name"].as_str().unwrap_or(""), v));
            }
        }
        Ok(json!({ "variables": out }))
    }

    fn evaluate(&mut self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("");
        let context = args["context"].as_str().unwrap_or("repl");
        let (t, r) = match self.frame(args).map(|(t, f)| (t, f["callFrameId"].clone())) {
            Some((t, frame)) => (
                t,
                self.call(
                    t,
                    "Debugger.evaluateOnCallFrame",
                    json!({
                        "callFrameId": frame, "expression": expression, "generatePreview": true,
                        "includeCommandLineAPI": context == "repl", "silent": context != "repl", "throwOnSideEffect": context == "hover",
                    }),
                )?,
            ),
            None => {
                let t = if self.targets.contains_key(&self.last_stopped) { self.last_stopped } else { MAIN };
                let params = json!({ "expression": expression, "generatePreview": true, "includeCommandLineAPI": true, "replMode": context == "repl", "silent": context != "repl" });
                (t, self.call(t, "Runtime.evaluate", params)?)
            }
        };
        if let Some(details) = r.get("exceptionDetails") {
            return Err(exception_text(details));
        }
        let v = self.variable(t, "", &r["result"]);
        Ok(json!({ "result": v["value"], "type": v["type"], "variablesReference": v["variablesReference"] }))
    }

    fn exception_info(&mut self, t: i64) -> Result<Value, String> {
        let paused = self.targets.get(&t).and_then(|x| x.paused.as_ref()).ok_or("Not paused")?;
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
        let reference = args["sourceReference"].as_i64().unwrap_or(0);
        let (t, script) = self.source_refs.get(&reference).cloned().ok_or("Unknown source")?;
        let r = self.call(t, "Debugger.getScriptSource", json!({ "scriptId": script }))?;
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
