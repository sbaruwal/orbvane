//! Testing, like the standard Test Explorer. The editor doesn't know how to find or run any
//! language's tests: a `TestProvider` does. Providers report
//! what they find and what happens as `TestEvent`s; the editor keeps them in a `TestTree`
//! (the tests and their latest results) and draws it (`workbench/testing_view.rs`: the
//! Testing view, gutter icons, failure messages, Test Results).
//!
//! Providers today: rust-analyzer's test explorer (`rust_analyzer.rs`), Go (`go.rs`: `go test
//! -json`) and pytest (`pytest.rs`). Others (extensions) implement the same trait.

pub mod go;
pub mod process;
pub mod pytest;
pub mod rust_analyzer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::runnables::DebugPlan;
use crate::servers::{ServerKey, Servers};

/// A test's result, like `TestResultState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TestState {
    #[default]
    Unset,
    Queued,
    Running,
    Passed,
    Failed,
    /// The test couldn't run ("errored"; for providers that can tell it apart from
    /// a failure, which rust-analyzer can't).
    #[allow(dead_code)]
    Errored,
    Skipped,
}

impl TestState {
    /// Which state a parent shows when its children differ.
    pub fn priority(self) -> u8 {
        match self {
            TestState::Running => 6,
            TestState::Errored => 5,
            TestState::Failed => 4,
            TestState::Queued => 3,
            TestState::Passed => 2,
            TestState::Skipped => 1,
            TestState::Unset => 0,
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, TestState::Failed | TestState::Errored)
    }
}

/// A test, or a group of them (a package, module or file).
#[derive(Clone, Debug, PartialEq)]
pub struct TestItem {
    /// Unique within its provider.
    pub id: String,
    pub label: String,
    pub parent: Option<String>,
    /// Where it's defined (0-based line).
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    /// It may have children (found by `TestProvider::discover`).
    pub has_children: bool,
    /// `TestProvider::debug` can debug it.
    pub debuggable: bool,
}

/// Why a test failed, and where (0-based line).
#[derive(Clone, Debug, PartialEq)]
pub struct TestMessage {
    pub text: String,
    pub location: Option<(PathBuf, usize)>,
}

/// Which tests a discovery replaces (the ones it doesn't list again are gone).
#[derive(Clone, Debug, PartialEq)]
pub enum Replace {
    /// Only adds or updates.
    Nothing,
    /// The top-level items (and their descendants) not listed again; the rest stay.
    TopLevel,
    /// The descendants of these items.
    Subtrees(Vec<String>),
    /// The items defined in these files.
    Files(Vec<PathBuf>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TestEvent {
    Discovered { replace: Replace, items: Vec<TestItem> },
    State { id: String, state: TestState, message: Option<TestMessage> },
    /// A line of the run's output (may have ANSI colors).
    Output(String),
    RunEnded,
    /// Something went wrong that the user should hear about.
    Error(String),
}

/// What a provider can use.
pub struct TestCx<'a> {
    pub lsp: &'a mut Servers,
    /// The workspace folder.
    pub root: &'a Path,
}

/// A language server's traffic, for providers built on one.
pub enum ServerEvent<'a> {
    Response { id: i64, result: &'a Result<Value, String> },
    Notification { method: &'a str, params: &'a Value },
}

/// Finds and runs one kind of tests.
pub trait TestProvider {
    /// Shown in messages ("rust-analyzer").
    fn name(&self) -> &str;
    /// Called every frame. `wanted`: the user is looking at tests, so start what finding
    /// them needs (a language server). Returns what happened since.
    fn tick(&mut self, cx: &mut TestCx, wanted: bool) -> Vec<TestEvent>;
    /// Finds the tests again: all of them (None), or the children of an item.
    fn discover(&mut self, cx: &mut TestCx, parent: Option<&str>);
    /// Runs these tests (empty: all of them). Results arrive as events, ending in `RunEnded`.
    fn run(&mut self, cx: &mut TestCx, include: &[String]) -> Result<(), String>;
    fn cancel(&mut self, cx: &mut TestCx);
    /// How to debug test `id` (None: it can't be).
    fn debug(&self, id: &str) -> Option<DebugPlan>;
    /// A language server's answer or notification (for providers built on one).
    fn server_event(&mut self, _cx: &mut TestCx, _key: &ServerKey, _event: ServerEvent) -> Vec<TestEvent> {
        Vec::new()
    }
    /// Files changed on disk (saved, or changed outside the editor).
    fn files_changed(&mut self, _paths: &[PathBuf]) {}
    /// Whether its tests get "run test | debug test" code lenses.
    fn code_lenses(&self) -> bool {
        false
    }
}

/// The providers for a workspace folder.
pub fn providers_for(root: &Path, waker: &lsp::Waker) -> Vec<Box<dyn TestProvider>> {
    let mut out: Vec<Box<dyn TestProvider>> = Vec::new();
    if let Some(p) = rust_analyzer::RustAnalyzerTests::detect(root) {
        out.push(Box::new(p));
    }
    if let Some(p) = go::GoTests::detect(root, waker) {
        out.push(Box::new(p));
    }
    if let Some(p) = pytest::PytestTests::detect(root, waker) {
        out.push(Box::new(p));
    }
    out
}

/// A test in the tree: (provider index, id).
pub type Key = (usize, String);

#[derive(Clone, Debug, Default)]
pub struct TestResult {
    pub state: TestState,
    pub message: Option<TestMessage>,
    started: Option<Instant>,
    pub duration: Option<Duration>,
}

/// Every provider's tests and their latest results.
#[derive(Default)]
pub struct TestTree {
    items: HashMap<Key, TestItem>,
    /// Children in display order (None: the top level), rebuilt after changes.
    children: HashMap<Option<Key>, Vec<Key>>,
    results: HashMap<Key, TestResult>,
    /// States shown for items (a group shows its children's), rebuilt after changes.
    shown: HashMap<Key, TestState>,
    dirty: bool,
    /// Counts changes to the items (for what's computed from them, like code lenses).
    pub version: u64,
}

impl TestTree {
    pub fn apply(&mut self, p: usize, replace: Replace, items: Vec<TestItem>) {
        self.version += 1;
        match replace {
            Replace::Nothing => {}
            Replace::TopLevel => {
                let gone: Vec<Key> = self.roots().iter().filter(|k| k.0 == p && !items.iter().any(|it| it.id == k.1)).cloned().collect();
                for k in gone {
                    for d in self.descendants(&k) {
                        self.items.remove(&d);
                    }
                    self.items.remove(&k);
                }
            }
            Replace::Subtrees(ids) => {
                for id in ids {
                    let gone = self.descendants(&(p, id));
                    for k in gone {
                        self.items.remove(&k);
                    }
                }
            }
            Replace::Files(files) => self.items.retain(|k, it| k.0 != p || !it.path.as_ref().is_some_and(|f| files.contains(f))),
        }
        for it in items {
            self.items.insert((p, it.id.clone()), it);
        }
        self.reindex();
    }

    fn reindex(&mut self) {
        let mut children: HashMap<Option<Key>, Vec<Key>> = HashMap::new();
        for (k, it) in &self.items {
            let parent = it.parent.as_ref().map(|id| (k.0, id.clone())).filter(|pk| self.items.contains_key(pk));
            // Children whose parent is gone (not rediscovered yet) are hidden.
            if parent.is_none() && it.parent.is_some() {
                continue;
            }
            children.entry(parent).or_default().push(k.clone());
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                let (x, y) = (&self.items[a], &self.items[b]);
                (x.path.is_some(), &x.path, x.line, &x.label, &a.1).cmp(&(y.path.is_some(), &y.path, y.line, &y.label, &b.1))
            });
        }
        self.children = children;
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn roots(&self) -> &[Key] {
        self.children.get(&None).map_or(&[], Vec::as_slice)
    }

    pub fn children(&self, key: &Key) -> &[Key] {
        self.children.get(&Some(key.clone())).map_or(&[], Vec::as_slice)
    }

    pub fn item(&self, key: &Key) -> Option<&TestItem> {
        self.items.get(key)
    }

    pub fn result(&self, key: &Key) -> Option<&TestResult> {
        self.results.get(key)
    }

    /// Everything under `key` (not itself).
    pub fn descendants(&self, key: &Key) -> Vec<Key> {
        let mut out = Vec::new();
        let mut stack = vec![key.clone()];
        while let Some(k) = stack.pop() {
            for c in self.children(&k) {
                out.push(c.clone());
                stack.push(c.clone());
            }
        }
        out
    }

    /// The tests (not groups) at or under `key`.
    pub fn tests_under(&self, key: &Key) -> Vec<Key> {
        let mut all = self.descendants(key);
        all.push(key.clone());
        all.retain(|k| self.items.get(k).is_some_and(|it| !it.has_children));
        all
    }

    /// The items defined in `path`.
    pub fn in_file<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a Key, &'a TestItem)> + 'a {
        self.items.iter().filter(move |(_, it)| it.path.as_deref() == Some(path))
    }

    /// Every item's key.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.items.keys()
    }

    /// Sets a test's result (starting a run: Queued; the provider's reports after that).
    pub fn set_state(&mut self, key: Key, state: TestState, message: Option<TestMessage>) {
        let r = self.results.entry(key).or_default();
        match state {
            TestState::Queued => {
                r.started = None;
                r.duration = None;
                r.message = None;
            }
            TestState::Running => r.started = Some(Instant::now()),
            _ => {
                if let Some(t) = r.started.take() {
                    r.duration = Some(t.elapsed());
                }
            }
        }
        r.state = state;
        if message.is_some() {
            r.message = message;
        }
        self.dirty = true;
    }

    /// A run ended: tests it never got to lose their queued/running marks.
    pub fn end_run(&mut self) {
        for r in self.results.values_mut() {
            if matches!(r.state, TestState::Queued | TestState::Running) {
                r.state = TestState::Unset;
                r.started = None;
            }
        }
        self.dirty = true;
    }

    /// The state drawn for `key`: its own result, or for a group the most important of its
    /// children's.
    pub fn shown(&mut self, key: &Key) -> TestState {
        if self.dirty {
            self.dirty = false;
            self.shown.clear();
            let roots = self.roots().to_vec();
            for k in roots {
                self.compute(&k);
            }
        }
        self.shown.get(key).copied().unwrap_or_default()
    }

    fn compute(&mut self, key: &Key) -> TestState {
        let own = self.results.get(key).map_or(TestState::Unset, |r| r.state);
        let kids = self.children(key).to_vec();
        let mut state = own;
        for k in kids {
            let s = self.compute(&k);
            if s.priority() > state.priority() {
                state = s;
            }
        }
        self.shown.insert(key.clone(), state);
        state
    }

    /// Results of tests (not groups) with a state: (passed, failed, skipped, all).
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for (k, r) in &self.results {
            if self.items.get(k).is_some_and(|it| it.has_children) || r.state == TestState::Unset {
                continue;
            }
            c.3 += 1;
            match r.state {
                TestState::Passed => c.0 += 1,
                TestState::Failed | TestState::Errored => c.1 += 1,
                TestState::Skipped => c.2 += 1,
                _ => {}
            }
        }
        c
    }

    /// Tests whose last result was a failure.
    pub fn failed(&self) -> Vec<Key> {
        self.results.iter().filter(|(_, r)| r.state.is_failure()).map(|(k, _)| k.clone()).collect()
    }

    /// Failure messages located in `path`: (line, message).
    pub fn messages_in(&self, path: &Path) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (k, r) in &self.results {
            if !r.state.is_failure() {
                continue;
            }
            let Some(m) = &r.message else { continue };
            let at = m.location.clone().or_else(|| self.items.get(k).and_then(|it| Some((it.path.clone()?, it.line?))));
            if let Some((_, line)) = at.filter(|(p, _)| p == path) {
                out.push((line, m.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("Test failed").trim().to_string()));
            }
        }
        out.sort();
        out.dedup_by_key(|m| m.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, parent: Option<&str>, line: Option<usize>, group: bool) -> TestItem {
        TestItem {
            id: id.into(),
            label: id.rsplit("::").next().unwrap().into(),
            parent: parent.map(String::from),
            path: line.map(|_| PathBuf::from("/p/src/lib.rs")),
            line,
            has_children: group,
            debuggable: !group,
        }
    }

    fn key(id: &str) -> Key {
        (0, id.to_string())
    }

    #[test]
    fn builds_the_tree_and_shows_the_worst_state() {
        let mut t = TestTree::default();
        t.apply(0, Replace::TopLevel, vec![item("demo", None, None, true)]);
        t.apply(
            0,
            Replace::Subtrees(vec!["demo".into()]),
            vec![item("demo::tests", Some("demo"), Some(3), true), item("demo::tests::b", Some("demo::tests"), Some(9), false), item("demo::tests::a", Some("demo::tests"), Some(5), false)],
        );
        assert_eq!(t.roots(), &[key("demo")]);
        // By location.
        assert_eq!(t.children(&key("demo::tests")), &[key("demo::tests::a"), key("demo::tests::b")]);
        assert_eq!(t.tests_under(&key("demo")).len(), 2);

        t.set_state(key("demo::tests::a"), TestState::Queued, None);
        t.set_state(key("demo::tests::b"), TestState::Queued, None);
        t.set_state(key("demo::tests::a"), TestState::Running, None);
        assert_eq!(t.shown(&key("demo")), TestState::Running);
        t.set_state(key("demo::tests::a"), TestState::Passed, None);
        let msg = TestMessage { text: "assertion failed\n  left: 1".into(), location: Some((PathBuf::from("/p/src/lib.rs"), 10)) };
        t.set_state(key("demo::tests::b"), TestState::Failed, Some(msg));
        assert_eq!(t.shown(&key("demo")), TestState::Failed);
        assert_eq!(t.shown(&key("demo::tests::a")), TestState::Passed);
        assert_eq!(t.counts(), (1, 1, 0, 2));
        assert_eq!(t.failed(), vec![key("demo::tests::b")]);
        assert_eq!(t.messages_in(Path::new("/p/src/lib.rs")), vec![(10, "assertion failed".to_string())]);
        assert!(t.result(&key("demo::tests::a")).unwrap().duration.is_some());

        // Rediscovering the file drops the test that's gone; its parent stays.
        t.apply(0, Replace::Files(vec![PathBuf::from("/p/src/lib.rs")]), vec![item("demo::tests", Some("demo"), Some(3), true), item("demo::tests::a", Some("demo::tests"), Some(5), false)]);
        assert_eq!(t.children(&key("demo::tests")), &[key("demo::tests::a")]);

        // A new top level keeps the subtrees of the items it lists again.
        t.apply(0, Replace::TopLevel, vec![item("demo", None, None, true), item("it", None, None, true)]);
        assert_eq!(t.roots(), &[key("demo"), key("it")]);
        assert_eq!(t.children(&key("demo::tests")), &[key("demo::tests::a")]);
        t.apply(0, Replace::TopLevel, vec![item("it", None, None, true)]);
        assert_eq!(t.roots(), &[key("it")]);
        assert!(t.item(&key("demo::tests::a")).is_none());
        t.apply(0, Replace::TopLevel, vec![item("demo", None, None, true)]);
        t.apply(0, Replace::Subtrees(vec!["demo".into()]), vec![item("demo::tests", Some("demo"), Some(3), true), item("demo::tests::a", Some("demo::tests"), Some(5), false)]);

        // A run that never reached a queued test clears it.
        t.set_state(key("demo::tests::a"), TestState::Queued, None);
        t.end_run();
        assert_eq!(t.shown(&key("demo::tests::a")), TestState::Unset);
    }

    #[test]
    fn orphans_are_hidden_until_their_parent_returns() {
        let mut t = TestTree::default();
        t.apply(0, Replace::Nothing, vec![item("demo::tests::a", Some("demo::tests"), Some(5), false)]);
        assert!(t.roots().is_empty());
        t.apply(0, Replace::Nothing, vec![item("demo::tests", None, Some(3), true)]);
        assert_eq!(t.children(&key("demo::tests")), &[key("demo::tests::a")]);
    }
}

/// Drives a provider against real tools in tests.
#[cfg(test)]
pub(crate) mod drive {
    use super::*;

    /// Ticks `p` until `done` is true of the events so far (or 60s pass); all the events.
    pub fn until(p: &mut dyn TestProvider, root: &Path, mut done: impl FnMut(&[TestEvent]) -> bool) -> Vec<TestEvent> {
        let mut lsp = Servers::new(std::sync::Arc::new(|| {}));
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done(&events) {
            assert!(Instant::now() < deadline, "timed out; events so far: {events:#?}");
            let mut cx = TestCx { lsp: &mut lsp, root };
            events.extend(p.tick(&mut cx, true));
            std::thread::sleep(Duration::from_millis(20));
        }
        events
    }

    pub fn run(p: &mut dyn TestProvider, root: &Path, include: &[String]) -> Vec<TestEvent> {
        let mut lsp = Servers::new(std::sync::Arc::new(|| {}));
        p.run(&mut TestCx { lsp: &mut lsp, root }, include).unwrap();
        until(p, root, |e| e.contains(&TestEvent::RunEnded))
    }

    /// The last state reported for `id`.
    pub fn state(events: &[TestEvent], id: &str) -> Option<(TestState, Option<TestMessage>)> {
        events.iter().rev().find_map(|e| match e {
            TestEvent::State { id: i, state, message } if i == id => Some((*state, message.clone())),
            _ => None,
        })
    }

    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbvane-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }
}

