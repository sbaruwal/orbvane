//! JavaScript and TypeScript tests with Jest or Vitest (whichever the folder's `package.json`
//! uses, run from its `node_modules`). Tests are found by reading the test files
//! (`*.test.*`, `*.spec.*`, `__tests__/`): `describe`, `it` and `test` calls with literal names.
//! Runs write the JSON report both tools share (`testResults[].assertionResults[]`), read when
//! the run ends; tests only known from a run (`test.each`, computed names) are added then.
//! Jest tests can be debugged (`--runInBand` in the Node debugger).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use serde_json::{json, Value};

use super::process::{Line, Proc};
use super::rust_analyzer::strip_ansi;
use super::{Replace, TestCx, TestEvent, TestItem, TestMessage, TestProvider, TestState};
use crate::runnables::{self, DebugPlan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Jest,
    Vitest,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Jest => "jest",
            Tool::Vitest => "vitest",
        }
    }
}

/// Which tool `root` uses: its config file, else its `package.json` (dependencies or the
/// `test` script).
fn detect_tool(root: &Path) -> Option<Tool> {
    let has_config = |stem: &str| {
        std::fs::read_dir(root).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("{stem}.config."))))
    };
    if has_config("vitest") {
        return Some(Tool::Vitest);
    }
    if has_config("jest") {
        return Some(Tool::Jest);
    }
    let package: Value = serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).ok()?).ok()?;
    let uses = |name: &str| {
        ["dependencies", "devDependencies"].iter().any(|d| package[d].get(name).is_some())
            || package["scripts"]["test"].as_str().is_some_and(|s| s.split_whitespace().any(|w| w == name))
    };
    if uses("vitest") {
        Some(Tool::Vitest)
    } else if uses("jest") || package.get("jest").is_some() {
        Some(Tool::Jest)
    } else {
        None
    }
}

const EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts"];

/// Whether `rel` (a path inside the folder) is a test file.
fn is_test_file(rel: &Path) -> bool {
    let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    if !EXTENSIONS.contains(&ext) {
        return false;
    }
    stem.ends_with(".test") || stem.ends_with(".spec") || rel.components().any(|c| c.as_os_str() == "__tests__")
}

/// The test files under `root` (relative), leaving out dependencies, build output and hidden
/// folders.
fn test_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&rel)) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = rel.join(&name);
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                if !name.starts_with('.') && !matches!(name.as_str(), "node_modules" | "dist" | "build" | "coverage" | "out") {
                    stack.push(path);
                }
            } else if is_test_file(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// A `describe` (group) or test call found in a file: its name, 0-based line, and the
/// groups it's in.
#[derive(Debug, PartialEq)]
struct Found {
    names: Vec<String>,
    line: usize,
    group: bool,
}

/// The `describe`/`it`/`test` calls with literal names in a test file's text. Nesting follows
/// brackets; comments, strings and templates are skipped.
fn find_tests(text: &str) -> Vec<Found> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    // Open groups: (name, bracket depth of the call).
    let mut groups: Vec<(String, usize)> = Vec::new();
    let (mut i, mut line, mut depth) = (0, 0, 0usize);
    // The end of a string starting at `i` (quote `q`), counting its line breaks.
    let skip_string = |i: usize, q: u8, line: &mut usize| -> usize {
        let mut j = i + 1;
        while j < b.len() && b[j] != q {
            if b[j] == b'\\' {
                j += 1;
            }
            if j < b.len() && b[j] == b'\n' {
                *line += 1;
            }
            j += 1;
        }
        j + 1
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => line += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = text[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                line += text[i..end].matches('\n').count();
                i = end;
                continue;
            }
            b'"' | b'\'' | b'`' => {
                i = skip_string(i, c, &mut line);
                continue;
            }
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth = depth.saturating_sub(1),
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    i += 1;
                }
                let word = &text[start..i];
                let after_dot = text[..start].trim_end().ends_with('.');
                if after_dot || !matches!(word, "describe" | "it" | "test" | "suite") {
                    continue;
                }
                // Modifiers: `.only`, `.skip`, `.concurrent`...; `.each` names are templates.
                let mut j = i;
                let mut each = false;
                while text[j..].starts_with('.') {
                    let k = j + 1 + text[j + 1..].find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(text.len() - j - 1);
                    each |= &text[j + 1..k] == "each";
                    j = k;
                }
                let rest = text[j..].trim_start();
                let Some(args) = rest.strip_prefix('(') else { continue };
                let args = args.trim_start();
                let Some(q) = args.bytes().next().filter(|q| matches!(q, b'"' | b'\'' | b'`')) else { continue };
                let ab = args.as_bytes();
                let mut end = 1;
                while end < ab.len() && ab[end] != q {
                    end += if ab[end] == b'\\' { 2 } else { 1 };
                }
                let Some(name) = args.get(1..end) else { continue };
                if each || name.contains('\n') || (q == b'`' && name.contains("${")) {
                    continue;
                }
                while groups.last().is_some_and(|g| g.1 >= depth) {
                    groups.pop();
                }
                let mut names: Vec<String> = groups.iter().map(|g| g.0.clone()).collect();
                names.push(name.replace("\\'", "'").replace("\\\"", "\""));
                let group = matches!(word, "describe" | "suite");
                if group {
                    groups.push((names.last().unwrap().clone(), depth));
                }
                out.push(Found { names, line, group });
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// A file's id; its tests' are `file::group::name`.
fn file_id(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

/// The items for the test files: folders, files, groups and tests.
fn items(root: &Path, files: &[PathBuf]) -> Vec<TestItem> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for rel in files {
        let mut parent: Option<String> = None;
        let parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        for k in 0..parts.len() - 1 {
            let id = format!("dir:{}", parts[..=k].join("/"));
            if seen.insert(id.clone()) {
                out.push(TestItem { id: id.clone(), label: parts[k].clone(), parent: parent.clone(), path: None, line: None, has_children: true, debuggable: false });
            }
            parent = Some(id);
        }
        let path = root.join(rel);
        let file = file_id(rel);
        out.push(TestItem { id: file.clone(), label: parts[parts.len() - 1].clone(), parent, path: Some(path.clone()), line: Some(0), has_children: true, debuggable: true });
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for f in find_tests(&text) {
            let id = format!("{file}::{}", f.names.join("::"));
            if !seen.insert(id.clone()) {
                continue;
            }
            let parent = if f.names.len() > 1 { format!("{file}::{}", f.names[..f.names.len() - 1].join("::")) } else { file.clone() };
            let label = f.names.last().unwrap().clone();
            out.push(TestItem { id, label, parent: Some(parent), path: Some(path.clone()), line: Some(f.line), has_children: f.group, debuggable: true });
        }
    }
    out
}

/// Escapes `s` for a regular expression.
fn regex_escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if "\\^$.|?*+()[]{}/".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// The first `file:line` in a failure's stack that's in `file`, as a 0-based line.
fn failure_line(message: &str, file: &Path) -> Option<usize> {
    let file = file.to_string_lossy();
    message.lines().find_map(|l| {
        let at = l.find(file.as_ref())?;
        let rest = &l[at + file.len()..];
        rest.strip_prefix(':')?.split(':').next()?.parse::<usize>().ok().map(|n| n.saturating_sub(1))
    })
}

/// The events for a JSON report: results, failure messages, and items for tests the files
/// didn't show (`known` holds the ids found so far).
fn report_events(report: &Value, root: &Path, known: &mut HashSet<String>) -> Vec<TestEvent> {
    let mut out = Vec::new();
    for file in report["testResults"].as_array().into_iter().flatten() {
        let Some(path) = file["name"].as_str().map(PathBuf::from) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let fid = file_id(&rel);
        let tests = file["assertionResults"].as_array().cloned().unwrap_or_default();
        let message = file["message"].as_str().map(strip_ansi).filter(|m| !m.trim().is_empty());
        if tests.is_empty() {
            // The file couldn't run (a syntax error, a failing import).
            if let Some(text) = message {
                let location = failure_line(&text, &path).map(|l| (path.clone(), l));
                out.push(TestEvent::State { id: fid.clone(), state: TestState::Errored, message: Some(TestMessage { text, location }) });
            }
            continue;
        }
        for t in tests {
            let mut names: Vec<String> = t["ancestorTitles"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
            names.push(t["title"].as_str().unwrap_or("").to_string());
            let id = format!("{fid}::{}", names.join("::"));
            let line = t["location"]["line"].as_u64().map(|l| (l as usize).saturating_sub(1));
            // Tests (and groups) only known from the run.
            let mut parent = fid.clone();
            for (k, name) in names.iter().enumerate() {
                let node = format!("{parent}::{name}");
                if known.insert(node.clone()) {
                    let leaf = k == names.len() - 1;
                    let item = TestItem { id: node.clone(), label: name.clone(), parent: Some(parent.clone()), path: Some(path.clone()), line: if leaf { line } else { None }, has_children: !leaf, debuggable: true };
                    out.push(TestEvent::Discovered { replace: Replace::Nothing, items: vec![item] });
                }
                parent = node;
            }
            let state = match t["status"].as_str() {
                Some("passed") => TestState::Passed,
                Some("failed") => TestState::Failed,
                _ => TestState::Skipped,
            };
            let failures: Vec<String> = t["failureMessages"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(strip_ansi)).collect();
            let message = (!failures.is_empty()).then(|| {
                let text = failures.join("\n\n").trim().to_string();
                let location = failure_line(&text, &path).or(line).map(|l| (path.clone(), l));
                TestMessage { text, location }
            });
            out.push(TestEvent::State { id, state, message });
        }
    }
    out
}

pub struct JestTests {
    root: PathBuf,
    tool: Tool,
    waker: lsp::Waker,
    collected: bool,
    recollect: bool,
    finding: Option<Receiver<Vec<TestItem>>>,
    known: HashSet<String>,
    /// Test files by id, and the groups and tests by id (their file).
    files: HashMap<String, PathBuf>,
    run: Option<(Proc, PathBuf)>,
}

impl JestTests {
    pub fn detect(root: &Path, waker: &lsp::Waker) -> Option<Self> {
        let tool = detect_tool(root)?;
        Some(JestTests {
            root: root.to_path_buf(),
            tool,
            waker: waker.clone(),
            collected: false,
            recollect: false,
            finding: None,
            known: HashSet::new(),
            files: HashMap::new(),
            run: None,
        })
    }

    fn binary(&self) -> PathBuf {
        self.root.join("node_modules/.bin").join(self.tool.name())
    }

    fn collect(&mut self) {
        if self.finding.is_some() {
            return;
        }
        self.collected = true;
        self.recollect = false;
        let (tx, rx) = mpsc::channel();
        let (root, waker) = (self.root.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let _ = tx.send(items(&root, &test_files(&root)));
            waker();
        });
        self.finding = Some(rx);
    }

    /// The file a test, group or file id is in.
    fn file_of(&self, id: &str) -> Option<&PathBuf> {
        self.files.get(id.split("::").next()?)
    }

    /// The test name pattern for test or group `id` (None: a whole file or folder).
    fn pattern(id: &str) -> Option<String> {
        let (_, names) = id.split_once("::")?;
        let full = names.split("::").collect::<Vec<_>>().join(" ");
        Some(format!("^{}( |$)", regex_escape(&full)))
    }
}

impl TestProvider for JestTests {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn tick(&mut self, _cx: &mut TestCx, wanted: bool) -> Vec<TestEvent> {
        let mut out = Vec::new();
        if (wanted && !self.collected) || self.recollect {
            self.collect();
        }
        if let Some(items) = self.finding.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.finding = None;
            self.known = items.iter().map(|i| i.id.clone()).collect();
            self.files = items.iter().filter(|i| !i.id.contains("::") && !i.id.starts_with("dir:")).filter_map(|i| Some((i.id.clone(), i.path.clone()?))).collect();
            out.push(TestEvent::Discovered { replace: Replace::TopLevel, items });
        }
        let Some((proc, report)) = &mut self.run else { return out };
        let mut done = false;
        for line in proc.poll() {
            match line {
                Line::Out(l) => out.push(TestEvent::Output(l)),
                Line::Done(_) => done = true,
            }
        }
        if done {
            let report = report.clone();
            self.run = None;
            match std::fs::read_to_string(&report).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
                Some(json) => out.extend(report_events(&json, &self.root, &mut self.known)),
                None => out.push(TestEvent::Error(format!("{} didn't write its report; see Test Results.", self.tool.name()))),
            }
            let _ = std::fs::remove_file(&report);
            out.push(TestEvent::RunEnded);
        }
        out
    }

    fn discover(&mut self, _cx: &mut TestCx, _parent: Option<&str>) {
        self.recollect = true;
    }

    fn run(&mut self, _cx: &mut TestCx, include: &[String]) -> Result<(), String> {
        if self.run.is_some() {
            return Err(format!("{} is busy.", self.tool.name()));
        }
        let bin = self.binary();
        if !bin.exists() {
            return Err(format!("{} isn't installed in this folder: run npm install first.", self.tool.name()));
        }
        // Files (or folders) to run, and test names when only tests were asked for.
        let mut paths = Vec::new();
        let mut patterns = Vec::new();
        let mut whole = include.is_empty();
        for id in include {
            if let Some(dir) = id.strip_prefix("dir:") {
                paths.push(dir.to_string());
                whole = true;
                continue;
            }
            let Some(file) = self.file_of(id) else { continue };
            let rel = file.strip_prefix(&self.root).unwrap_or(file).to_string_lossy().into_owned();
            if !paths.contains(&rel) {
                paths.push(rel);
            }
            match Self::pattern(id) {
                Some(p) => patterns.push(p),
                None => whole = true,
            }
        }
        let report = std::env::temp_dir().join(format!("orbvane-{}-{}-{}.json", self.tool.name(), std::process::id(), self.known.len()));
        let _ = std::fs::remove_file(&report);
        let mut args: Vec<String> = match self.tool {
            Tool::Jest => vec!["--json".into(), format!("--outputFile={}", report.display()), "--testLocationInResults".into()],
            Tool::Vitest => vec!["run".into(), "--reporter=default".into(), "--reporter=json".into(), format!("--outputFile.json={}", report.display())],
        };
        if self.tool == Tool::Jest && !paths.is_empty() && !include.iter().any(|i| i.starts_with("dir:")) {
            args.push("--runTestsByPath".into());
        }
        args.extend(paths);
        if !whole && !patterns.is_empty() {
            args.push("-t".into());
            args.push(patterns.join("|"));
        }
        let env = [("FORCE_COLOR", "0"), ("CI", "1")];
        let proc = Proc::spawn(&bin, &args, &self.root, &env, self.waker.clone()).map_err(|e| format!("Couldn't run {}: {e}", bin.display()))?;
        self.run = Some((proc, report));
        Ok(())
    }

    fn cancel(&mut self, _cx: &mut TestCx) {
        if let Some((proc, report)) = self.run.take() {
            proc.kill();
            let _ = std::fs::remove_file(report);
        }
    }

    fn debug(&self, id: &str) -> Option<DebugPlan> {
        // Vitest runs tests in worker processes the debugger doesn't follow.
        if self.tool != Tool::Jest {
            return None;
        }
        let file = self.file_of(id)?;
        let mut args = vec![json!("--runInBand"), json!("--runTestsByPath"), json!(file)];
        if let Some(p) = Self::pattern(id) {
            args.extend([json!("-t"), json!(p)]);
        }
        let label = id.rsplit("::").next().unwrap_or(id).to_string();
        let config = json!({
            "type": "node", "request": "launch", "name": format!("Debug {label}"),
            "program": self.root.join("node_modules/jest/bin/jest.js"), "args": args, "cwd": self.root,
        });
        Some(runnables::direct(label, self.root.clone(), config))
    }

    fn files_changed(&mut self, paths: &[PathBuf]) {
        let relevant = |p: &PathBuf| {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            !rel.components().any(|c| c.as_os_str() == "node_modules") && (is_test_file(rel) || name == "package.json")
        };
        if self.collected && paths.iter().any(|p| p.starts_with(&self.root) && relevant(p)) {
            self.recollect = true;
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
    fn finds_tests_in_files() {
        let text = "import { add } from './add';\n// it('commented out')\ndescribe('add', () => {\n  it(\"adds\", () => {\n    expect(add(1, 2)).toBe(3);\n  });\n  test.skip('it\\'s skipped', () => {});\n  describe.each([1, 2])('n=%i', (n) => {});\n  const s = 'test(\"in a string\")';\n});\n/* test('in a comment')\n*/\ntest(`top level`, async () => {});\nit.each([1])('each %i', () => {});\n";
        let found = find_tests(text);
        let got: Vec<(String, usize, bool)> = found.iter().map(|f| (f.names.join(" > "), f.line, f.group)).collect();
        assert_eq!(
            got,
            [("add".into(), 2, true), ("add > adds".into(), 3, false), ("add > it's skipped".into(), 6, false), ("top level".into(), 12, false)]
        );
        assert!(is_test_file(Path::new("src/add.test.ts")) && is_test_file(Path::new("src/__tests__/add.js")) && !is_test_file(Path::new("src/add.ts")));
        assert_eq!(JestTests::pattern("src/a.test.js::add (1)::adds"), Some("^add \\(1\\) adds( |$)".into()));
        assert_eq!(JestTests::pattern("src/a.test.js"), None);
    }

    #[test]
    fn reads_reports() {
        let root = Path::new("/p");
        let report = json!({ "testResults": [
            { "name": "/p/src/a.test.js", "message": "", "assertionResults": [
                { "ancestorTitles": ["add"], "title": "adds", "status": "passed", "failureMessages": [], "location": { "line": 4, "column": 3 } },
                { "ancestorTitles": ["add"], "title": "n=2", "status": "failed", "failureMessages": ["Error: expect(received).toBe(expected)\n    at Object.<anonymous> (/p/src/a.test.js:9:20)"], "location": null },
                { "ancestorTitles": [], "title": "later", "status": "pending", "failureMessages": [] },
            ]},
            { "name": "/p/src/b.test.js", "message": "\u{1b}[1mSyntaxError: Unexpected token (3:1)\u{1b}[22m", "assertionResults": [] },
        ]});
        let mut known: HashSet<String> = ["src/a.test.js::add".to_string(), "src/a.test.js::add::adds".to_string()].into();
        let events = report_events(&report, root, &mut known);
        let state = |id: &str| super::super::drive::state(&events, id);
        assert_eq!(state("src/a.test.js::add::adds"), Some((TestState::Passed, None)));
        let (s, m) = state("src/a.test.js::add::n=2").unwrap();
        assert_eq!((s, m.as_ref().unwrap().location.clone()), (TestState::Failed, Some((PathBuf::from("/p/src/a.test.js"), 8))));
        assert_eq!(state("src/a.test.js::later").unwrap().0, TestState::Skipped);
        let (s, m) = state("src/b.test.js").unwrap();
        assert_eq!((s, m.unwrap().text), (TestState::Errored, "SyntaxError: Unexpected token (3:1)".to_string()));
        // The test only the run knew about was added under its group.
        let added: Vec<(&str, Option<&str>)> = events.iter().filter_map(|e| match e {
            TestEvent::Discovered { items, .. } => Some((items[0].id.as_str(), items[0].parent.as_deref())),
            _ => None,
        }).collect();
        assert_eq!(added, [("src/a.test.js::add::n=2", Some("src/a.test.js::add")), ("src/a.test.js::later", Some("src/a.test.js"))]);
    }

    /// A stand-in for the tool: a script in `node_modules/.bin` that writes a report.
    #[test]
    fn runs_a_tool_and_reads_its_report() {
        use crate::testing::drive;
        use std::os::unix::fs::PermissionsExt;

        let root = drive::scratch("jest");
        std::fs::write(root.join("package.json"), r#"{ "devDependencies": { "jest": "^30.0.0" } }"#).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.test.js"), "describe('add', () => {\n  it('adds', () => {});\n  it('fails', () => {});\n});\n").unwrap();
        let bin = root.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        let report = format!(
            r#"{{"testResults":[{{"name":"{}/src/a.test.js","message":"","assertionResults":[{{"ancestorTitles":["add"],"title":"adds","status":"passed","failureMessages":[]}},{{"ancestorTitles":["add"],"title":"fails","status":"failed","failureMessages":["boom"]}}]}}]}}"#,
            root.display()
        );
        // Writes the report where --outputFile says and echoes its arguments.
        let script = format!("#!/bin/sh\necho \"args: $*\"\nfor a in \"$@\"; do case \"$a\" in --outputFile=*) printf '%s' '{report}' > \"${{a#--outputFile=}}\";; esac; done\n");
        std::fs::write(bin.join("jest"), script).unwrap();
        std::fs::set_permissions(bin.join("jest"), std::fs::Permissions::from_mode(0o755)).unwrap();

        let waker: lsp::Waker = std::sync::Arc::new(|| {});
        let mut p = JestTests::detect(&root, &waker).unwrap();
        assert_eq!(p.tool, Tool::Jest);
        let found = drive::until(&mut p, &root, |e| e.iter().any(|e| matches!(e, TestEvent::Discovered { .. })));
        let Some(TestEvent::Discovered { items, .. }) = found.last() else { panic!() };
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["dir:src", "src/a.test.js", "src/a.test.js::add", "src/a.test.js::add::adds", "src/a.test.js::add::fails"]);

        let events = drive::run(&mut p, &root, &["src/a.test.js::add::fails".into()]);
        let output: Vec<&String> = events.iter().filter_map(|e| if let TestEvent::Output(l) = e { Some(l) } else { None }).collect();
        assert!(output.iter().any(|l| l.contains("--runTestsByPath src/a.test.js -t ^add fails( |$)")), "{output:?}");
        assert_eq!(drive::state(&events, "src/a.test.js::add::adds").unwrap().0, TestState::Passed);
        let (state, message) = drive::state(&events, "src/a.test.js::add::fails").unwrap();
        assert_eq!((state, message.unwrap().text), (TestState::Failed, "boom".to_string()));
        assert_eq!(p.debug("src/a.test.js::add::fails").unwrap().config["args"], json!(["--runInBand", "--runTestsByPath", root.join("src/a.test.js"), "-t", "^add fails( |$)"]));
    }
}
