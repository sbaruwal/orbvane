//! Rust tests through rust-analyzer's test explorer (its `experimental/discoverTest`,
//! `runTest`, `changeTestState`... extension, the one its editor extension uses): the server
//! finds the tests (packages → modules → tests), runs them with cargo and reports each test's
//! state and the output. rust-analyzer leaves failure messages empty, so the panic in the
//! output ("thread 'tests::fails' panicked at src/lib.rs:9:18:" and the lines after it)
//! becomes the failed test's message and location.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use language::Lang;
use serde_json::{json, Value};

use super::{Replace, ServerEvent, TestCx, TestEvent, TestItem, TestMessage, TestProvider, TestState};
use crate::runnables::{self, DebugPlan};
use crate::servers::{ServerKey, Servers};

/// Asking again when the server couldn't answer (still loading).
const RETRY: Duration = Duration::from_secs(2);

enum Req {
    Discover(Option<String>),
    Run,
}

pub struct RustAnalyzerTests {
    key: ServerKey,
    /// The top level has been asked for (and when to ask again, if the answer was empty
    /// because the workspace was still loading).
    discovered: bool,
    retry: Option<Instant>,
    work_done: u64,
    pending: HashMap<i64, Req>,
    /// Tests' runnables (to debug them).
    runnables: HashMap<String, Value>,
    /// A panic being read from the output: the test's name in its crate and its message.
    panic: Option<(String, TestMessage)>,
    /// Failure messages by the test's name in its crate, until its failed state arrives.
    panics: HashMap<String, TestMessage>,
    /// Failed tests whose message hasn't been read yet (the output and the states come from
    /// different streams, so either can come first): name → id.
    awaiting: HashMap<String, String>,
    unavailable: bool,
    running: bool,
}

impl RustAnalyzerTests {
    /// A provider for Cargo workspaces (a Cargo.toml in `root`) when rust-analyzer exists.
    pub fn detect(root: &Path) -> Option<Self> {
        if !root.join("Cargo.toml").is_file() {
            return None;
        }
        let key = Servers::key_of(Lang::Rust, root)?;
        Some(RustAnalyzerTests {
            key,
            discovered: false,
            retry: None,
            work_done: 0,
            pending: HashMap::new(),
            runnables: HashMap::new(),
            panic: None,
            panics: HashMap::new(),
            awaiting: HashMap::new(),
            unavailable: false,
            running: false,
        })
    }

    fn items(&mut self, result: &Value) -> Vec<TestItem> {
        let mut out = Vec::new();
        for t in result["tests"].as_array().into_iter().flatten() {
            let Some(id) = t["id"].as_str() else { continue };
            let path = t["textDocument"]["uri"].as_str().and_then(lsp::uri_to_path);
            let line = t["range"]["start"]["line"].as_u64().map(|l| l as usize);
            let runnable = &t["runnable"];
            if !runnable.is_null() {
                self.runnables.insert(id.to_string(), runnable.clone());
            }
            out.push(TestItem {
                id: id.to_string(),
                label: t["label"].as_str().unwrap_or(id).to_string(),
                parent: t["parent"].as_str().map(String::from),
                path,
                line,
                has_children: t["kind"].as_str() != Some("test") || t["canResolveChildren"].as_bool() == Some(true),
                debuggable: !runnable.is_null(),
            });
        }
        out
    }

    /// A line of output: panics become failure messages (and a failed test's state gets its
    /// message when the failure was reported first).
    fn read_output(&mut self, line: &str, root: &Path) -> Vec<TestEvent> {
        let plain = strip_ansi(line);
        if let Some((name, file, at)) = parse_panic(&plain) {
            let done = self.finish_panic();
            let path = root.join(file);
            self.panic = Some((name, TestMessage { text: String::new(), location: Some((path, at)) }));
            return done;
        }
        let ends = plain.is_empty()
            || plain.starts_with("stack backtrace:")
            || plain.starts_with("note: ")
            || plain.starts_with("failures:")
            || plain.starts_with("---- ")
            || line.starts_with('\x1b');
        if ends {
            return self.finish_panic();
        }
        if let Some((_, msg)) = &mut self.panic {
            if !msg.text.is_empty() {
                msg.text.push('\n');
            }
            msg.text.push_str(&plain);
        }
        Vec::new()
    }

    /// The panic being read is complete: it's the message of its test.
    fn finish_panic(&mut self) -> Vec<TestEvent> {
        let Some((name, msg)) = self.panic.take() else { return Vec::new() };
        match self.awaiting.remove(&name) {
            Some(id) => vec![TestEvent::State { id, state: TestState::Failed, message: Some(msg) }],
            None => {
                self.panics.insert(name, msg);
                Vec::new()
            }
        }
    }

    /// The failure message of test `id` if the output has had it; else it's awaited.
    fn message_for(&mut self, id: &str) -> Option<TestMessage> {
        // Ids start with the crate or test target ("demo::tests::fails"); threads are named
        // by the path within it ("tests::fails").
        let name = id.split_once("::").map_or(id, |(_, rest)| rest);
        let msg = self.panics.remove(name);
        if msg.is_none() {
            self.awaiting.insert(name.to_string(), id.to_string());
        }
        msg
    }
}

impl TestProvider for RustAnalyzerTests {
    fn name(&self) -> &str {
        "rust-analyzer"
    }

    fn tick(&mut self, cx: &mut TestCx, wanted: bool) -> Vec<TestEvent> {
        match cx.lsp.ready(&self.key) {
            None => {
                if wanted && !self.unavailable {
                    self.unavailable = true;
                    return vec![TestEvent::Error("rust-analyzer isn't available, so tests can't be found.".into())];
                }
                return Vec::new();
            }
            Some(false) => {
                if wanted && !cx.lsp.is_started(&self.key) {
                    cx.lsp.start(Lang::Rust, cx.root);
                }
                return Vec::new();
            }
            Some(true) => {}
        }
        self.unavailable = false;
        // Asked again after the server finishes work (loading the workspace, checking after a
        // save): an answer given while it was loading can miss packages and test targets.
        let due = self.retry.is_some_and(|t| Instant::now() >= t) || (self.work_done != cx.lsp.work_done && !self.running);
        if !self.discovered || due {
            self.discovered = true;
            self.retry = None;
            self.work_done = cx.lsp.work_done;
            self.discover(cx, None);
        }
        Vec::new()
    }

    fn discover(&mut self, cx: &mut TestCx, parent: Option<&str>) {
        if let Some(id) = cx.lsp.ext_request(&self.key, "experimental/discoverTest", json!({ "testId": parent })) {
            self.pending.insert(id, Req::Discover(parent.map(String::from)));
        }
    }

    fn run(&mut self, cx: &mut TestCx, include: &[String]) -> Result<(), String> {
        if cx.lsp.ready(&self.key) != Some(true) {
            return Err("rust-analyzer is still starting.".into());
        }
        self.panics.clear();
        self.awaiting.clear();
        self.panic = None;
        let include = if include.is_empty() { Value::Null } else { json!(include) };
        let id = cx.lsp.ext_request(&self.key, "experimental/runTest", json!({ "include": include, "exclude": null })).ok_or("rust-analyzer isn't running.")?;
        self.pending.insert(id, Req::Run);
        self.running = true;
        Ok(())
    }

    fn cancel(&mut self, cx: &mut TestCx) {
        self.running = false;
        cx.lsp.ext_notify(&self.key, "experimental/abortRunTest", Value::Null);
    }

    fn debug(&self, id: &str) -> Option<DebugPlan> {
        runnables::debug_plan(self.runnables.get(id)?)
    }

    fn server_event(&mut self, cx: &mut TestCx, key: &ServerKey, event: ServerEvent) -> Vec<TestEvent> {
        if *key != self.key {
            return Vec::new();
        }
        match event {
            ServerEvent::Response { id, result } => match (self.pending.remove(&id), result) {
                (Some(Req::Discover(parent)), Ok(v)) => {
                    let items = self.items(v);
                    if parent.is_none() && items.is_empty() {
                        // Still loading the workspace: ask again when its work is done.
                        self.retry = Some(Instant::now() + RETRY * 5);
                        return Vec::new();
                    }
                    let mut events = Vec::new();
                    // The top level lists packages and test targets; their subtrees come in
                    // one answer each, so ask for them all now (the gutter needs them).
                    if parent.is_none() {
                        let groups: Vec<String> = items.iter().filter(|i| i.has_children).map(|i| i.id.clone()).collect();
                        events.push(TestEvent::Discovered { replace: Replace::TopLevel, items });
                        for g in groups {
                            self.discover(cx, Some(&g));
                        }
                    } else {
                        let replace = Replace::Subtrees(parent.into_iter().collect());
                        events.push(TestEvent::Discovered { replace, items });
                    }
                    events
                }
                (Some(Req::Discover(parent)), Err(_)) => {
                    if parent.is_none() {
                        self.retry = Some(Instant::now() + RETRY);
                    }
                    Vec::new()
                }
                (Some(Req::Run), Err(e)) => {
                    self.running = false;
                    vec![TestEvent::Error(format!("Running the tests failed: {e}")), TestEvent::RunEnded]
                }
                _ => Vec::new(),
            },
            ServerEvent::Notification { method, params } => match method {
                "experimental/discoveredTests" => {
                    let items = self.items(params);
                    let replace = if let Some(scope) = params["scope"].as_array() {
                        Replace::Subtrees(scope.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    } else if let Some(files) = params["scopeFile"].as_array().filter(|f| !f.is_empty()) {
                        Replace::Files(files.iter().filter_map(|f| f["uri"].as_str().and_then(lsp::uri_to_path)).collect())
                    } else {
                        Replace::Nothing
                    };
                    if items.is_empty() && replace == Replace::Nothing {
                        return Vec::new();
                    }
                    vec![TestEvent::Discovered { replace, items }]
                }
                "experimental/changeTestState" => {
                    let Some(id) = params["testId"].as_str() else { return Vec::new() };
                    let st = &params["state"];
                    let state = match st["tag"].as_str() {
                        Some("started") => TestState::Running,
                        Some("passed") => TestState::Passed,
                        Some("failed") => TestState::Failed,
                        Some("skipped") => TestState::Skipped,
                        Some("enqueued") => TestState::Queued,
                        _ => return Vec::new(),
                    };
                    let message = if state == TestState::Failed {
                        let given = st["message"].as_str().filter(|m| !m.trim().is_empty()).map(|m| TestMessage { text: strip_ansi(m), location: None });
                        self.message_for(id).or(given)
                    } else {
                        None
                    };
                    vec![TestEvent::State { id: id.to_string(), state, message }]
                }
                "experimental/appendOutputToRunTest" => {
                    let line = params.as_str().unwrap_or_default().to_string();
                    let root = self.key.1.clone();
                    let mut events = vec![TestEvent::Output(line.clone())];
                    events.extend(self.read_output(&line, &root));
                    events
                }
                "experimental/endRunTest" => {
                    self.running = false;
                    let mut events = self.finish_panic();
                    // Failures whose output never said why.
                    for (_, id) in self.awaiting.drain() {
                        events.push(TestEvent::State { id, state: TestState::Failed, message: Some(TestMessage { text: "Test failed".into(), location: None }) });
                    }
                    events.push(TestEvent::RunEnded);
                    events
                }
                _ => Vec::new(),
            },
        }
    }
}

/// "thread 'tests::fails' (8564182) panicked at src/lib.rs:9:18:" → the thread's name, the
/// file and the 0-based line.
fn parse_panic(line: &str) -> Option<(String, PathBuf, usize)> {
    let rest = line.strip_prefix("thread '")?;
    let (name, rest) = rest.split_once('\'')?;
    let (_, at) = rest.split_once(" panicked at ")?;
    let at = at.trim_end().trim_end_matches(':');
    let mut parts = at.rsplitn(3, ':');
    let _col = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let file = parts.next()?;
    Some((name.to_string(), PathBuf::from(file), line.saturating_sub(1)))
}

/// `s` without ANSI escape sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_panics() {
        assert_eq!(parse_panic("thread 'tests::fails' (8564182) panicked at src/lib.rs:9:18:"), Some(("tests::fails".into(), PathBuf::from("src/lib.rs"), 8)));
        assert_eq!(parse_panic("thread 'main' panicked at tests/it.rs:3:5:"), Some(("main".into(), PathBuf::from("tests/it.rs"), 2)));
        assert_eq!(parse_panic("running 3 tests"), None);
        assert_eq!(strip_ansi("\x1b[1m\x1b[92m   Compiling\x1b[0m demo"), "   Compiling demo");
    }

    /// The failure can be reported before its panic output is read (different streams): the
    /// message follows when the output arrives.
    #[test]
    fn failure_messages_in_either_order() {
        let dir = std::env::temp_dir().join(format!("orbvane-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"d\"\n").unwrap();
        let mut p = RustAnalyzerTests::detect(&dir).unwrap();
        let mut lsp = Servers::new(std::sync::Arc::new(|| {}));
        let key = p.key.clone();
        let mut send = |p: &mut RustAnalyzerTests, method: &str, params: Value| {
            let mut cx = TestCx { lsp: &mut lsp, root: &dir };
            p.server_event(&mut cx, &key, ServerEvent::Notification { method, params: &params })
        };
        let failed = json!({ "testId": "d::tests::fails", "state": { "tag": "failed", "message": "" } });
        // Failure first: no message yet.
        let ev = send(&mut p, "experimental/changeTestState", failed.clone());
        assert!(matches!(&ev[..], [TestEvent::State { message: None, .. }]), "{ev:?}");
        send(&mut p, "experimental/appendOutputToRunTest", json!("thread 'tests::fails' (1) panicked at src/lib.rs:9:18:"));
        send(&mut p, "experimental/appendOutputToRunTest", json!("math is broken"));
        let ev = send(&mut p, "experimental/appendOutputToRunTest", json!(""));
        let Some(TestEvent::State { id, message: Some(m), .. }) = ev.iter().find(|e| matches!(e, TestEvent::State { .. })) else { panic!("{ev:?}") };
        assert_eq!((id.as_str(), m.text.as_str(), m.location.clone()), ("d::tests::fails", "math is broken", Some((dir.join("src/lib.rs"), 8))));
        // Output first: the failure comes with it.
        send(&mut p, "experimental/appendOutputToRunTest", json!("thread 'tests::fails' (1) panicked at src/lib.rs:9:18:"));
        send(&mut p, "experimental/appendOutputToRunTest", json!("again"));
        let ev = send(&mut p, "experimental/changeTestState", failed);
        let ev2 = send(&mut p, "experimental/endRunTest", Value::Null);
        let all: Vec<&TestEvent> = ev.iter().chain(&ev2).collect();
        assert!(all.iter().any(|e| matches!(e, TestEvent::State { message: Some(m), .. } if m.text == "again")), "{all:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finds and runs a crate's tests with a real rust-analyzer (skipped without one).
    #[test]
    fn finds_and_runs_tests() {
        use crate::servers::Event;
        use crate::testing::{Key, TestTree};

        let dir = std::env::temp_dir().join(format!("orbvane-tests-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"tdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn add(a: u32, b: u32) -> u32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn adds() { assert_eq!(add(1, 2), 3); }\n    #[test]\n    fn fails() { assert_eq!(add(1, 2), 4, \"math is broken\"); }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("tests/it.rs"), "#[test]\nfn integration() { assert!(tdemo::add(2, 2) == 4); }\n").unwrap();

        let mut lsp = Servers::new(std::sync::Arc::new(|| {}));
        let mut p = RustAnalyzerTests::detect(&dir).unwrap();
        let mut tree = TestTree::default();
        let mut ended = false;
        // Runs the provider and the server until `done` (or a timeout).
        let pump = |lsp: &mut Servers, p: &mut RustAnalyzerTests, tree: &mut TestTree, ended: &mut bool, done: &dyn Fn(&TestTree, bool) -> bool| -> bool {
            let deadline = Instant::now() + Duration::from_secs(180);
            while Instant::now() < deadline {
                let mut cx = TestCx { lsp, root: &dir };
                let mut events = p.tick(&mut cx, true);
                if cx.lsp.ready(&p.key).is_none() {
                    return false;
                }
                for e in cx.lsp.poll() {
                    let mut cx = TestCx { lsp, root: &dir };
                    events.extend(match &e {
                        Event::ExtResponse { key, id, result } => p.server_event(&mut cx, key, ServerEvent::Response { id: *id, result }),
                        Event::ExtNotification { key, method, params } => p.server_event(&mut cx, key, ServerEvent::Notification { method, params }),
                        _ => Vec::new(),
                    });
                }
                for ev in events {
                    match ev {
                        TestEvent::Discovered { replace, items } => tree.apply(0, replace, items),
                        TestEvent::State { id, state, message } => tree.set_state((0, id), state, message),
                        TestEvent::RunEnded => *ended = true,
                        _ => {}
                    }
                }
                if done(tree, *ended) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("timed out");
        };
        let key = |id: &str| -> Key { (0, id.to_string()) };
        if !pump(&mut lsp, &mut p, &mut tree, &mut ended, &|t, _| t.item(&(0, "tdemo::tests::fails".into())).is_some() && t.item(&(0, "it::integration".into())).is_some()) {
            eprintln!("rust-analyzer not installed; skipping");
            return;
        }
        let fails = tree.item(&key("tdemo::tests::fails")).unwrap();
        assert_eq!((fails.line, fails.debuggable), (Some(8), true));
        assert!(p.debug("tdemo::tests::fails").is_some_and(|plan| plan.build.is_some_and(|b| b.contains("--no-run"))));

        let mut cx = TestCx { lsp: &mut lsp, root: &dir };
        p.run(&mut cx, &[]).unwrap();
        pump(&mut lsp, &mut p, &mut tree, &mut ended, &|_, ended| ended);
        assert_eq!(tree.shown(&key("tdemo::tests::adds")), TestState::Passed);
        assert_eq!(tree.shown(&key("it::integration")), TestState::Passed);
        assert_eq!(tree.shown(&key("tdemo")), TestState::Failed);
        let failure = tree.result(&key("tdemo::tests::fails")).unwrap();
        assert_eq!(failure.state, TestState::Failed);
        let msg = failure.message.as_ref().unwrap();
        assert!(msg.text.contains("math is broken"), "{msg:?}");
        assert_eq!(msg.location, Some((dir.join("src/lib.rs"), 8)));
        lsp.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
