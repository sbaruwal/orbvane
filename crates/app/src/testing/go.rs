//! Go tests: `*_test.go` files are scanned for `TestXxx`,
//! `BenchmarkXxx`, `ExampleXxx` and `FuzzXxx` functions (packages → files → functions), runs
//! are `go test -json` (subtests show up as they run), and debugging is Delve in test mode.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use serde_json::{json, Value};

use super::process::{Line, Proc};
use super::{Replace, TestCx, TestEvent, TestItem, TestMessage, TestProvider, TestState};
use crate::runnables::{self, DebugPlan};

/// A test function found in a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    pub name: String,
    /// 0-based.
    pub line: usize,
}

/// A `_test.go` file: its package line and test functions.
#[derive(Clone, Debug, PartialEq)]
pub struct TestFile {
    pub package_line: usize,
    pub funcs: Vec<Func>,
}

const KINDS: [&str; 4] = ["Test", "Benchmark", "Example", "Fuzz"];

/// The test functions in a `_test.go` file's text (`func TestXxx(t *testing.T)`...; the
/// letter after the prefix can't be lowercase, as `go test` requires).
pub fn scan_file(text: &str) -> TestFile {
    let mut package_line = 0;
    let mut seen_package = false;
    let mut funcs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if !seen_package && line.starts_with("package ") {
            package_line = i;
            seen_package = true;
        }
        let Some(rest) = line.strip_prefix("func ") else { continue };
        let Some(paren) = rest.find('(') else { continue };
        let name = rest[..paren].trim();
        let Some(kind) = KINDS.iter().find(|k| name.starts_with(**k)) else { continue };
        if name.chars().nth(kind.len()).is_some_and(|c| c.is_lowercase()) || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let params = rest[paren + 1..].split(')').next().unwrap_or("").trim();
        // Examples take nothing; the others take their *testing.T/B/F.
        if (*kind == "Example") != params.is_empty() {
            continue;
        }
        funcs.push(Func { name: name.to_string(), line: i });
    }
    TestFile { package_line, funcs }
}

/// The module path in go.mod.
fn module_path(root: &Path) -> String {
    std::fs::read_to_string(root.join("go.mod"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("module ").map(|m| m.trim().trim_matches('"').to_string())))
        .unwrap_or_default()
}

/// Every `_test.go` file under `root`, relative ("pkg/x_test.go"), skipping what `go` skips.
fn test_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with('_') {
                continue;
            }
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if !matches!(name.as_str(), "vendor" | "testdata" | "node_modules") && !path.join("go.mod").is_file() {
                    dirs.push(path);
                }
            } else if name.ends_with("_test.go") {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    out.sort();
    out
}

/// A package's folder relative to the root ("." for the root).
fn package_of(rel_file: &str) -> String {
    match rel_file.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => ".".into(),
    }
}

fn package_id(rel: &str) -> String {
    format!("pkg:{rel}")
}

fn test_id(rel: &str, name: &str) -> String {
    format!("test:{rel}:{name}")
}

/// `go test`'s `-run` pattern for a test or subtest ("A/b c" → `^A$/^b_c$`).
fn run_pattern(name: &str) -> String {
    name.split('/').map(|part| format!("^{}$", quote_meta(part))).collect::<Vec<_>>().join("/")
}

fn quote_meta(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// The line of `t.Run("name", ...)` after line `from`, for a subtest as `go test` names it
/// (spaces became `_`, repeats got `#01`...).
fn subtest_line(text: &str, from: usize, name: &str) -> Option<usize> {
    let name = match name.rsplit_once('#') {
        Some((base, n)) if n.chars().all(|c| c.is_ascii_digit()) => base,
        _ => name,
    };
    let spaced = name.replace('_', " ");
    text.lines().enumerate().skip(from + 1).find_map(|(i, l)| {
        let (_, rest) = l.split_once(".Run(")?;
        let lit = rest.trim_start().strip_prefix('"')?.split('"').next()?;
        (lit == name || lit == spaced).then_some(i)
    })
}

/// The scanned tests as items.
fn items(root: &Path, module: &str, files: &[(String, TestFile)]) -> Vec<TestItem> {
    let mut out = Vec::new();
    let mut packages = HashSet::new();
    for (rel, file) in files {
        if file.funcs.is_empty() {
            continue;
        }
        let pkg = package_of(rel);
        if packages.insert(pkg.clone()) {
            let label = if pkg == "." { module.to_string() } else if module.is_empty() { pkg.clone() } else { format!("{module}/{pkg}") };
            out.push(TestItem { id: package_id(&pkg), label, parent: None, path: None, line: None, has_children: true, debuggable: true });
        }
        let path = root.join(rel);
        let file_id = format!("file:{rel}");
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        out.push(TestItem { id: file_id.clone(), label: name, parent: Some(package_id(&pkg)), path: Some(path.clone()), line: Some(file.package_line), has_children: true, debuggable: true });
        for f in &file.funcs {
            out.push(TestItem {
                id: test_id(&pkg, &f.name),
                label: f.name.clone(),
                parent: Some(file_id.clone()),
                path: Some(path.clone()),
                line: Some(f.line),
                has_children: false,
                debuggable: true,
            });
        }
    }
    out
}

/// What one `go test` invocation runs: a package, and the patterns of its tests and
/// benchmarks (both None: the whole package).
#[derive(Debug, PartialEq)]
struct Invocation {
    package: String,
    run: Option<String>,
    bench: Option<String>,
}

impl Invocation {
    fn args(&self) -> Vec<String> {
        let mut args = vec!["test".to_string(), "-json".to_string()];
        match (&self.run, &self.bench) {
            (Some(r), _) => args.extend(["-run".into(), r.clone()]),
            (None, Some(_)) => args.extend(["-run".into(), "^$".into()]),
            (None, None) => {}
        }
        if let Some(b) = &self.bench {
            args.extend(["-bench".into(), b.clone()]);
        }
        args.push(if self.package == "." { "./".to_string() } else { format!("./{}", self.package) });
        args
    }
}

/// Splits the ids to run into `go test` invocations, one per package (and one per subtest).
fn plan(ids: &[String], files: &HashMap<String, Vec<String>>) -> Vec<Invocation> {
    let mut whole = Vec::new();
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut subtests = Vec::new();
    for id in ids {
        if let Some(pkg) = id.strip_prefix("pkg:") {
            whole.push(pkg.to_string());
        } else if let Some(rel) = id.strip_prefix("file:") {
            names.entry(package_of(rel)).or_default().extend(files.get(rel).cloned().unwrap_or_default());
        } else if let Some((pkg, name)) = id.strip_prefix("test:").and_then(|r| r.split_once(':')) {
            if name.contains('/') {
                subtests.push((pkg.to_string(), name.to_string()));
            } else {
                names.entry(pkg.to_string()).or_default().push(name.to_string());
            }
        }
    }
    let mut out: Vec<Invocation> = whole.iter().map(|p| Invocation { package: p.clone(), run: None, bench: None }).collect();
    for (pkg, list) in names {
        if whole.contains(&pkg) {
            continue;
        }
        let pattern = |v: Vec<&String>| (!v.is_empty()).then(|| format!("^({})$", v.iter().map(|n| quote_meta(n)).collect::<Vec<_>>().join("|")));
        let (bench, tests): (Vec<&String>, Vec<&String>) = list.iter().partition(|n| n.starts_with("Benchmark"));
        out.push(Invocation { package: pkg, run: pattern(tests), bench: pattern(bench) });
    }
    for (pkg, name) in subtests {
        if !whole.contains(&pkg) {
            out.push(Invocation { package: pkg, run: Some(run_pattern(&name)), bench: None });
        }
    }
    out
}

/// A failed test's output as its message: the lines it printed (not `go test`'s own), and the
/// first `file_test.go:12:` location in them.
fn failure(output: &str, dir: &Path) -> TestMessage {
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| !l.starts_with("=== ") && !l.trim_start().starts_with("--- "))
        .collect();
    let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let text: Vec<&str> = lines.iter().map(|l| l.get(indent..).unwrap_or(l.trim_start())).collect();
    let location = text.iter().find_map(|l| {
        let (file, rest) = l.trim_start().split_once(':')?;
        let line: usize = rest.split(':').next()?.parse().ok()?;
        (file.ends_with(".go") && !file.contains(' ')).then(|| (dir.join(file), line.saturating_sub(1)))
    });
    TestMessage { text: text.join("\n").trim_end().to_string(), location }
}

struct Run {
    proc: Proc,
    /// What's still to run.
    queue: VecDeque<Invocation>,
    /// Each test's output so far.
    output: HashMap<String, String>,
    /// Packages that reported a test.
    reported: HashSet<String>,
}

pub struct GoTests {
    root: PathBuf,
    module: String,
    go: PathBuf,
    waker: lsp::Waker,
    scanned: bool,
    rescan: bool,
    scanning: Option<Receiver<Vec<(String, TestFile)>>>,
    /// The test functions in each file ("pkg/x_test.go" → names).
    files: HashMap<String, Vec<String>>,
    /// Known items (subtests are added as they run).
    known: HashMap<String, TestItem>,
    run: Option<Run>,
}

impl GoTests {
    /// A provider for a Go module (a go.mod in `root`) when `go` is installed.
    pub fn detect(root: &Path, waker: &lsp::Waker) -> Option<Self> {
        if !root.join("go.mod").is_file() {
            return None;
        }
        let go = crate::servers::find_binary("go")?;
        Some(GoTests {
            root: root.to_path_buf(),
            module: module_path(root),
            go,
            waker: waker.clone(),
            scanned: false,
            rescan: false,
            scanning: None,
            files: HashMap::new(),
            known: HashMap::new(),
            run: None,
        })
    }

    fn start_scan(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        self.scanned = true;
        self.rescan = false;
        let (tx, rx) = mpsc::channel();
        let (root, waker) = (self.root.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let files = test_files(&root).into_iter().filter_map(|rel| Some((rel.clone(), scan_file(&std::fs::read_to_string(root.join(&rel)).ok()?)))).collect();
            let _ = tx.send(files);
            waker();
        });
        self.scanning = Some(rx);
    }

    fn start_next(&mut self) -> Result<(), String> {
        let Some(run) = &mut self.run else { return Ok(()) };
        let Some(inv) = run.queue.pop_front() else { return Ok(()) };
        run.proc = Proc::spawn(&self.go, &inv.args(), &self.root, &[], self.waker.clone()).map_err(|e| format!("Couldn't run go test: {e}"))?;
        Ok(())
    }

    fn rel_package(&self, import_path: &str) -> String {
        if import_path == self.module {
            ".".into()
        } else {
            import_path.strip_prefix(&format!("{}/", self.module)).unwrap_or(import_path).to_string()
        }
    }

    /// One line of `go test -json`.
    fn json_line(&mut self, line: &str) -> Vec<TestEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![TestEvent::Output(line.to_string())] };
        let mut out = Vec::new();
        let pkg = self.rel_package(v["Package"].as_str().unwrap_or(""));
        let test = v["Test"].as_str();
        let id = test.map(|t| test_id(&pkg, t));
        let Some(run) = &mut self.run else { return out };
        match (v["Action"].as_str().unwrap_or(""), &id) {
            ("output", _) => {
                let text = v["Output"].as_str().unwrap_or("");
                if let Some(id) = &id {
                    run.output.entry(id.clone()).or_default().push_str(text);
                }
                out.push(TestEvent::Output(text.trim_end_matches('\n').to_string()));
            }
            ("run", Some(id)) => {
                run.reported.insert(pkg.clone());
                if !self.known.contains_key(id) {
                    // A subtest (or a test the scan missed): under its parent.
                    let name = test.unwrap_or("");
                    let parent = match name.rsplit_once('/') {
                        Some((p, _)) => test_id(&pkg, p),
                        None => package_id(&pkg),
                    };
                    let (path, from) = self.known.get(&parent).map_or((None, None), |p| (p.path.clone(), p.line));
                    // Its own `t.Run("name"` line, when the name is written out.
                    let leaf = name.rsplit('/').next().unwrap_or(name);
                    let line = path.as_ref().zip(from).and_then(|(path, from)| subtest_line(&std::fs::read_to_string(path).ok()?, from, leaf));
                    let label = name.rsplit('/').next().unwrap_or(name).to_string();
                    let item = TestItem { id: id.clone(), label, parent: Some(parent), path, line, has_children: false, debuggable: true };
                    if let Some(p) = self.known.get_mut(item.parent.as_ref().unwrap()) {
                        p.has_children = true;
                    }
                    self.known.insert(id.clone(), item.clone());
                    out.push(TestEvent::Discovered { replace: Replace::Nothing, items: vec![item] });
                }
                out.push(TestEvent::State { id: id.clone(), state: TestState::Running, message: None });
            }
            (action @ ("pass" | "fail" | "skip"), Some(id)) => {
                let state = match action {
                    "pass" => TestState::Passed,
                    "fail" => TestState::Failed,
                    _ => TestState::Skipped,
                };
                let message = (state == TestState::Failed).then(|| failure(run.output.get(id).map_or("", String::as_str), &self.root.join(&pkg)));
                out.push(TestEvent::State { id: id.clone(), state, message });
            }
            ("fail", None) if !run.reported.contains(&pkg) => {
                // The package didn't build: its output says why.
                out.push(TestEvent::Error(format!("go test failed for {}", v["Package"].as_str().unwrap_or(&pkg))));
            }
            _ => {}
        }
        out
    }
}

impl TestProvider for GoTests {
    fn name(&self) -> &str {
        "Go"
    }

    fn tick(&mut self, _cx: &mut TestCx, _wanted: bool) -> Vec<TestEvent> {
        let mut out = Vec::new();
        // Scanned without waiting to be wanted: the code lenses need the tests.
        if !self.scanned || self.rescan {
            self.start_scan();
        }
        if let Some(files) = self.scanning.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.scanning = None;
            let old: Vec<PathBuf> = self.files.keys().map(|f| self.root.join(f)).collect();
            self.files = files.iter().map(|(rel, f)| (rel.clone(), f.funcs.iter().map(|f| f.name.clone()).collect())).collect();
            let items = items(&self.root, &self.module, &files);
            self.known = items.iter().map(|it| (it.id.clone(), it.clone())).collect();
            let mut changed: Vec<PathBuf> = files.iter().map(|(rel, _)| self.root.join(rel)).collect();
            changed.extend(old);
            out.push(TestEvent::Discovered { replace: Replace::Files(changed), items: Vec::new() });
            out.push(TestEvent::Discovered { replace: Replace::TopLevel, items });
        }
        let lines = self.run.as_mut().map(|r| r.proc.poll()).unwrap_or_default();
        for line in lines {
            match line {
                Line::Out(l) => out.extend(self.json_line(&l)),
                Line::Done(_) => {
                    let more = self.run.as_ref().is_some_and(|r| !r.queue.is_empty());
                    if !more {
                        self.run = None;
                        out.push(TestEvent::RunEnded);
                    } else if let Err(e) = self.start_next() {
                        self.run = None;
                        out.push(TestEvent::Error(e));
                        out.push(TestEvent::RunEnded);
                    }
                }
            }
        }
        out
    }

    fn discover(&mut self, _cx: &mut TestCx, _parent: Option<&str>) {
        self.rescan = true;
    }

    fn run(&mut self, _cx: &mut TestCx, include: &[String]) -> Result<(), String> {
        let mut queue: VecDeque<Invocation> = if include.is_empty() {
            VecDeque::from([Invocation { package: "...".into(), run: None, bench: None }])
        } else {
            plan(include, &self.files).into()
        };
        let first = queue.pop_front().ok_or("Nothing to run.")?;
        let proc = Proc::spawn(&self.go, &first.args(), &self.root, &[], self.waker.clone()).map_err(|e| format!("Couldn't run go test: {e}"))?;
        self.run = Some(Run { proc, queue, output: HashMap::new(), reported: HashSet::new() });
        Ok(())
    }

    fn cancel(&mut self, _cx: &mut TestCx) {
        if let Some(run) = self.run.take() {
            run.proc.kill();
        }
    }

    fn debug(&self, id: &str) -> Option<DebugPlan> {
        let item = self.known.get(id)?;
        let (pkg, args) = if let Some(pkg) = id.strip_prefix("pkg:") {
            (pkg.to_string(), Vec::new())
        } else if let Some(rel) = id.strip_prefix("file:") {
            let names = self.files.get(rel)?;
            (package_of(rel), vec!["-test.run".to_string(), format!("^({})$", names.iter().map(|n| quote_meta(n)).collect::<Vec<_>>().join("|"))])
        } else {
            let (pkg, name) = id.strip_prefix("test:")?.split_once(':')?;
            let args = if name.starts_with("Benchmark") {
                vec!["-test.bench".to_string(), run_pattern(name), "-test.run".into(), "^$".into()]
            } else {
                vec!["-test.run".to_string(), run_pattern(name)]
            };
            (pkg.to_string(), args)
        };
        let dir = self.root.join(&pkg);
        let config = json!({ "type": "go", "request": "launch", "name": format!("Debug {}", item.label), "mode": "test", "program": dir, "args": args });
        Some(runnables::direct(item.label.clone(), self.root.clone(), config))
    }

    fn files_changed(&mut self, paths: &[PathBuf]) {
        if self.scanned && paths.iter().any(|p| p.starts_with(&self.root) && p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("_test.go") || n == "go.mod")) {
            self.rescan = true;
        }
    }

    fn code_lenses(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_test_functions() {
        let f = scan_file("// x\npackage demo\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {}\nfunc Testable(t *testing.T) {}\nfunc BenchmarkAdd(b *testing.B) {}\nfunc ExampleAdd() {}\nfunc ExampleBad(t int) {}\nfunc helper(t *testing.T) {}\nfunc (s *S) TestMethod(t *testing.T) {}\nfunc FuzzParse(f *testing.F) {}\n");
        assert_eq!(f.package_line, 1);
        let names: Vec<(&str, usize)> = f.funcs.iter().map(|f| (f.name.as_str(), f.line)).collect();
        assert_eq!(names, [("TestAdd", 5), ("BenchmarkAdd", 7), ("ExampleAdd", 8), ("FuzzParse", 12)]);
    }

    #[test]
    fn plans_invocations() {
        let files = HashMap::from([("a/x_test.go".to_string(), vec!["TestA".to_string(), "BenchmarkA".to_string()])]);
        let ids = ["file:a/x_test.go".to_string(), "test:.:TestRoot".to_string(), "test:.:TestRoot/sub case".to_string(), "pkg:b".to_string(), "test:b:TestB".to_string()];
        let p = plan(&ids, &files);
        assert_eq!(p[0], Invocation { package: "b".into(), run: None, bench: None });
        assert_eq!(p[1].args(), ["test", "-json", "-run", "^(TestRoot)$", "./"]);
        assert_eq!(p[2].args(), ["test", "-json", "-run", "^(TestA)$", "-bench", "^(BenchmarkA)$", "./a"]);
        assert_eq!(p[3].run.as_deref(), Some("^TestRoot$/^sub case$"));
        assert_eq!(p.len(), 4);
    }

    #[test]
    fn finds_subtest_lines() {
        let text = "func TestT(t *testing.T) {\n\tt.Run(\"a b\", func(t *testing.T) {})\n\tt.Run(name, f)\n}\n";
        assert_eq!(subtest_line(text, 0, "a_b"), Some(1));
        assert_eq!(subtest_line(text, 0, "a_b#01"), Some(1));
        assert_eq!(subtest_line(text, 0, "dynamic"), None);
    }

    #[test]
    fn reads_failures() {
        let out = "=== RUN   TestAdd\n    add_test.go:9: got 4, want 5\n        more detail\n--- FAIL: TestAdd (0.00s)\n";
        let m = failure(out, Path::new("/p/pkg"));
        assert_eq!(m.text, "add_test.go:9: got 4, want 5\n    more detail");
        assert_eq!(m.location, Some((PathBuf::from("/p/pkg/add_test.go"), 8)));
    }

    #[test]
    fn runs_real_go_tests() {
        use crate::testing::drive;
        if crate::servers::find_binary("go").is_none() {
            return eprintln!("no go; skipped");
        }
        let root = drive::scratch("gotests");
        std::fs::write(root.join("go.mod"), "module example.com/demo\n\ngo 1.21\n").unwrap();
        std::fs::create_dir_all(root.join("calc")).unwrap();
        std::fs::write(root.join("calc/calc.go"), "package calc\n\nfunc Add(a, b int) int { return a + b }\n").unwrap();
        std::fs::write(
            root.join("calc/calc_test.go"),
            "package calc\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(2, 2) != 4 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n\nfunc TestTable(t *testing.T) {\n\tt.Run(\"ok\", func(t *testing.T) {})\n\tt.Run(\"bad case\", func(t *testing.T) {\n\t\tt.Errorf(\"got %d\", Add(1, 1))\n\t})\n}\n",
        )
        .unwrap();
        let waker: lsp::Waker = std::sync::Arc::new(|| {});
        let mut p = GoTests::detect(&root, &waker).unwrap();
        let found = drive::until(&mut p, &root, |e| e.iter().any(|e| matches!(e, TestEvent::Discovered { replace: Replace::TopLevel, .. })));
        let Some(TestEvent::Discovered { items, .. }) = found.last() else { panic!() };
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["example.com/demo/calc", "calc_test.go", "TestAdd", "TestTable"]);

        let events = drive::run(&mut p, &root, &["file:calc/calc_test.go".into()]);
        assert_eq!(drive::state(&events, "test:calc:TestAdd").unwrap().0, TestState::Passed);
        assert_eq!(drive::state(&events, "test:calc:TestTable/ok").unwrap().0, TestState::Passed);
        let (state, message) = drive::state(&events, "test:calc:TestTable/bad_case").unwrap();
        assert_eq!(state, TestState::Failed);
        let message = message.unwrap();
        assert_eq!(message.text, "calc_test.go:14: got 2");
        assert_eq!(message.location, Some((root.join("calc/calc_test.go"), 13)));
        assert_eq!(drive::state(&events, "test:calc:TestTable").unwrap().0, TestState::Failed);
        // The subtests were discovered under their test.
        assert!(events.iter().any(|e| matches!(e, TestEvent::Discovered { items, .. } if items.iter().any(|i| i.id == "test:calc:TestTable/bad_case" && i.parent.as_deref() == Some("test:calc:TestTable")))));
        let plan = p.debug("test:calc:TestTable/bad_case").unwrap();
        assert!(plan.build.is_none());
        assert_eq!(plan.config["args"], json!(["-test.run", "^TestTable$/^bad_case$"]));
    }
}

