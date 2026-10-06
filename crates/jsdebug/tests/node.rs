//! Drives the adapter against a real `node` (skipped when there's none on the PATH).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use serde_json::{json, Value};

struct Session {
    tx: Sender<Value>,
    rx: Receiver<Value>,
    seq: i64,
    /// Events seen while waiting for responses.
    events: Vec<Value>,
}

fn node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).chain([PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")]).map(|d| d.join("node")).find(|p| p.is_file())
}

impl Session {
    fn start() -> Session {
        let (tx, adapter_rx) = mpsc::channel();
        let (adapter_tx, rx) = mpsc::channel();
        std::thread::spawn(move || jsdebug::serve(adapter_rx, adapter_tx));
        Session { tx, rx, seq: 0, events: Vec::new() }
    }

    fn next(&mut self) -> Value {
        let m = self.rx.recv_timeout(Duration::from_secs(20)).expect("the adapter went quiet");
        if std::env::var_os("TRACE").is_some() { eprintln!("<- {m}"); }
        m
    }

    fn request(&mut self, command: &str, arguments: Value) -> Value {
        self.seq += 1;
        let seq = self.seq;
        self.tx.send(json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })).unwrap();
        loop {
            let msg = self.next();
            if msg["type"] == "response" && msg["request_seq"] == seq {
                assert!(msg["success"].as_bool().unwrap(), "{command} failed: {msg}");
                return msg["body"].clone();
            }
            self.events.push(msg);
        }
    }

    fn event(&mut self, name: &str) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return self.events.remove(i)["body"].clone();
        }
        loop {
            let msg = self.next();
            if msg["event"] == name {
                return msg["body"].clone();
            }
            self.events.push(msg);
        }
    }

    fn output(&self) -> String {
        self.events.iter().filter(|e| e["event"] == "output").map(|e| e["body"]["output"].as_str().unwrap_or("")).collect()
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jsdebug-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn launch(s: &mut Session, node: &Path, program: &Path, bps: &[Value]) {
    s.request("initialize", json!({ "adapterID": "node" }));
    s.request("launch", json!({ "program": program, "runtimeExecutable": node, "skipFiles": ["<node_internals>/**"] }));
    s.event("initialized");
    let r = s.request("setBreakpoints", json!({ "source": { "path": program }, "breakpoints": bps }));
    assert!(r["breakpoints"].as_array().unwrap().iter().all(|b| b["verified"] == true), "{r}");
    s.request("configurationDone", json!({}));
}

#[test]
fn debugs_a_program() {
    let Some(node) = node() else { return eprintln!("no node; skipped") };
    let dir = scratch("basic");
    let program = dir.join("main.js");
    std::fs::write(&program, "function add(a, b) {\n  const sum = a + b;\n  return sum;\n}\nconst list = [1, 'two', { three: 3 }];\nfor (let i = 0; i < 3; i++) {\n  console.log('i =', i, list);\n}\nconsole.log('done', add(2, 3));\n").unwrap();
    let mut s = Session::start();
    launch(&mut s, &node, &program, &[json!({ "line": 3 }), json!({ "line": 7, "hitCondition": "2" }), json!({ "line": 9, "logMessage": "sum is {add(1, 1)}" })]);

    // The hit condition lets i = 0 and i = 2 pass.
    let stop = s.event("stopped");
    assert_eq!(stop["reason"], "breakpoint");
    let frames = s.request("stackTrace", json!({ "threadId": 1 }));
    assert_eq!(frames["stackFrames"][0]["line"], 7);
    let scopes = s.request("scopes", json!({ "frameId": 1 }));
    let block = scopes["scopes"][0]["variablesReference"].clone();
    let vars = s.request("variables", json!({ "variablesReference": block }));
    let i = vars["variables"].as_array().unwrap().iter().find(|v| v["name"] == "i").unwrap().clone();
    assert_eq!(i["value"], "1");
    let e = s.request("evaluate", json!({ "expression": "list", "frameId": 1, "context": "watch" }));
    assert_eq!(e["result"], "(3) [1, 'two', {…}]");
    let items = s.request("variables", json!({ "variablesReference": e["variablesReference"] }));
    assert_eq!(items["variables"][1]["value"], "'two'");

    s.request("continue", json!({ "threadId": 1 }));
    let stop = s.event("stopped");
    assert_eq!(stop["reason"], "breakpoint");
    let frames = s.request("stackTrace", json!({ "threadId": 1 }));
    assert_eq!((frames["stackFrames"][0]["name"].as_str(), frames["stackFrames"][0]["line"].as_i64()), (Some("add"), Some(3)));
    let local = s.request("scopes", json!({ "frameId": 1 }))["scopes"][0].clone();
    assert_eq!(local["name"], "Local");
    let vars = s.request("variables", json!({ "variablesReference": local["variablesReference"] }));
    let names: Vec<&str> = vars["variables"].as_array().unwrap().iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"a") && names.contains(&"sum"), "{names:?}");
    s.request("next", json!({ "threadId": 1 }));
    assert_eq!(s.event("stopped")["reason"], "step");
    s.request("continue", json!({ "threadId": 1 }));

    let exited = s.event("exited");
    assert_eq!(exited["exitCode"], 0);
    s.event("terminated");
    let out = s.output();
    assert!(out.contains("i = 0 (3) [1, 'two', {…}]") && out.contains("sum is 2") && out.contains("done 5"), "{out}");
    s.request("disconnect", json!({}));
}

#[test]
fn maps_typescript_sources() {
    let Some(node) = node() else { return eprintln!("no node; skipped") };
    let dir = scratch("maps");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    let ts = dir.join("src/app.ts");
    // Two lines of TypeScript and the JavaScript tsc makes from them, one to one.
    std::fs::write(&ts, "let a: number = 1;\nconsole.log(a);\n").unwrap();
    std::fs::write(dir.join("out/app.js"), "let a = 1;\nconsole.log(a);\n//# sourceMappingURL=app.js.map\n").unwrap();
    std::fs::write(dir.join("out/app.js.map"), r#"{"version":3,"file":"app.js","sources":["../src/app.ts"],"mappings":"AAAA,IAAI,CAAC,GAAW,CAAC,CAAC;AAClB,OAAO,CAAC,GAAG,CAAC,CAAC,CAAC,CAAC"}"#).unwrap();
    let mut s = Session::start();
    s.request("initialize", json!({ "adapterID": "node" }));
    s.request("launch", json!({ "program": dir.join("out/app.js"), "runtimeExecutable": node }));
    s.event("initialized");
    let r = s.request("setBreakpoints", json!({ "source": { "path": ts }, "breakpoints": [{ "line": 2 }] }));
    // Not bound until the script loads.
    assert_eq!(r["breakpoints"][0]["verified"], false);
    s.request("configurationDone", json!({}));
    let bound = s.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    assert_eq!(s.event("stopped")["reason"], "breakpoint");
    let frames = s.request("stackTrace", json!({ "threadId": 1 }));
    let top = &frames["stackFrames"][0];
    assert_eq!((top["source"]["path"].as_str(), top["line"].as_i64()), (ts.to_str(), Some(2)));
    s.request("continue", json!({ "threadId": 1 }));
    s.event("terminated");
    s.request("disconnect", json!({}));
}

/// Breakpoints hit in a child process the program starts and in a worker thread, each its own
/// thread.
#[test]
fn follows_child_processes_and_workers() {
    let Some(node) = node() else { return eprintln!("no node; skipped") };
    let dir = scratch("children");
    let main = dir.join("main.js");
    let child = dir.join("child.js");
    let worker = dir.join("worker.js");
    std::fs::write(&main, "const { execFileSync } = require('child_process');\nconst { Worker } = require('worker_threads');\nconst out = execFileSync(process.execPath, [__dirname + '/child.js']).toString();\nconsole.log('child said', out.trim());\nconst w = new Worker(__dirname + '/worker.js');\nw.on('message', (m) => { console.log('worker said', m); });\n").unwrap();
    std::fs::write(&child, "const word = 'hello';\nconsole.log(word);\n").unwrap();
    std::fs::write(&worker, "const { parentPort } = require('worker_threads');\nconst n = 6 * 7;\nparentPort.postMessage(n);\n").unwrap();
    let mut s = Session::start();
    s.request("initialize", json!({ "adapterID": "node" }));
    s.request("launch", json!({ "program": main, "runtimeExecutable": node, "skipFiles": ["<node_internals>/**"] }));
    s.event("initialized");
    s.request("setBreakpoints", json!({ "source": { "path": child }, "breakpoints": [{ "line": 2 }] }));
    s.request("setBreakpoints", json!({ "source": { "path": worker }, "breakpoints": [{ "line": 3 }] }));
    s.request("configurationDone", json!({}));

    // The child process: a thread of its own, stopped at its breakpoint.
    let started = s.event("thread");
    assert_eq!(started["reason"], "started");
    let stop = s.event("stopped");
    assert_eq!(stop["reason"], "breakpoint");
    let t = stop["threadId"].as_i64().unwrap();
    assert_ne!(t, 1);
    let threads = s.request("threads", json!({}));
    let names: Vec<&str> = threads["threads"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.iter().any(|n| n.starts_with("child.js [")), "{names:?}");
    let frames = s.request("stackTrace", json!({ "threadId": t }));
    let top = frames["stackFrames"][0].clone();
    assert_eq!((top["source"]["path"].as_str(), top["line"].as_i64()), (child.to_str(), Some(2)));
    let e = s.request("evaluate", json!({ "expression": "word", "frameId": top["id"], "context": "watch" }));
    assert_eq!(e["result"], "'hello'");
    s.request("continue", json!({ "threadId": t }));
    assert_eq!(s.event("thread")["reason"], "exited");

    // The worker.
    let started = s.event("thread");
    assert_eq!(started["reason"], "started");
    let stop = s.event("stopped");
    let w = stop["threadId"].as_i64().unwrap();
    let frames = s.request("stackTrace", json!({ "threadId": w }));
    let top = frames["stackFrames"][0].clone();
    assert_eq!((top["source"]["path"].as_str(), top["line"].as_i64()), (worker.to_str(), Some(3)));
    let scopes = s.request("scopes", json!({ "frameId": top["id"] }));
    let all: Vec<Value> = scopes["scopes"].as_array().unwrap().clone();
    let mut found = false;
    for scope in all.iter().filter(|s| s["expensive"] != true) {
        let vars = s.request("variables", json!({ "variablesReference": scope["variablesReference"] }));
        found |= vars["variables"].as_array().unwrap().iter().any(|v| v["name"] == "n" && v["value"] == "42");
    }
    assert!(found, "{scopes}");
    s.request("continue", json!({ "threadId": w }));

    assert_eq!(s.event("exited")["exitCode"], 0);
    s.event("terminated");
    let out = s.output();
    assert!(out.contains("child said hello") && out.contains("worker said 42"), "{out}");
    s.request("disconnect", json!({}));
}
