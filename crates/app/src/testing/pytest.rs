//! Python tests with pytest: `pytest --collect-only -q` finds them
//! (folders → files → classes → functions → parameters), `pytest -v` runs them (each result is
//! a line; failures are read from the FAILURES section), and debugpy debugs them
//! (`"module": "pytest"`). The interpreter is the workspace's virtual environment, if any.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::json;

use super::process::{Line, Proc};
use super::{Replace, TestCx, TestEvent, TestItem, TestMessage, TestProvider, TestState};
use crate::runnables::{self, DebugPlan};

/// The Python a workspace uses: its virtual environment (`.venv`, `venv`, `env`), else
/// `python3` on the PATH.
pub fn interpreter(root: &Path) -> PathBuf {
    [".venv", "venv", "env"]
        .iter()
        .map(|d| root.join(d).join("bin/python"))
        .find(|p| p.is_file())
        .or_else(|| crate::servers::find_binary("python3"))
        .unwrap_or_else(|| PathBuf::from("python3"))
}

/// Whether `root` looks like it has pytest tests: pytest's config, or test files at the top
/// or in `tests/`.
fn has_tests(root: &Path) -> bool {
    let contains = |file: &str, needle: &str| std::fs::read_to_string(root.join(file)).is_ok_and(|t| t.contains(needle));
    if root.join("pytest.ini").is_file() || root.join("conftest.py").is_file() || contains("pyproject.toml", "[tool.pytest") || contains("setup.cfg", "[tool:pytest]") || contains("tox.ini", "[pytest]") {
        return true;
    }
    [root.to_path_buf(), root.join("tests"), root.join("test")].iter().any(|d| {
        std::fs::read_dir(d).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.ends_with(".py") && (n.starts_with("test_") || n.ends_with("_test.py"))
            })
        })
    })
}

const PYTEST: [&str; 3] = ["-m", "pytest", "-p"];

fn pytest_args(extra: &[&str]) -> Vec<String> {
    PYTEST.iter().chain(&["no:cacheprovider", "--color=no"]).chain(extra).map(|s| s.to_string()).collect()
}

/// The 0-based line of `def name` (in `class`, when given) or `class name` in a file's text.
fn line_of(text: &str, class: Option<&str>, def: Option<&str>) -> Option<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let find = |from: usize, word: &str, name: &str| {
        (from..lines.len()).find(|&i| {
            let t = lines[i].trim_start();
            let t = t.strip_prefix("async ").unwrap_or(t);
            t.strip_prefix(word).and_then(|r| r.strip_prefix(name)).is_some_and(|r| r.starts_with('(') || r.starts_with(':'))
        })
    };
    let start = match class {
        Some(c) => find(0, "class ", c)?,
        None => 0,
    };
    match def {
        Some(d) => find(start, "def ", d),
        None => Some(start),
    }
}

/// Collected node ids as items: folders, files, classes, functions and their parameters.
fn items(root: &Path, nodeids: &[String]) -> Vec<TestItem> {
    let mut out: Vec<TestItem> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut add = |out: &mut Vec<TestItem>, item: TestItem| {
        if seen.insert(item.id.clone()) {
            out.push(item);
        }
    };
    for id in nodeids {
        let mut parts = id.split("::");
        let Some(file) = parts.next() else { continue };
        let rest: Vec<&str> = parts.collect();
        // Folders.
        let mut parent: Option<String> = None;
        let dirs: Vec<&str> = file.split('/').collect();
        for i in 0..dirs.len() - 1 {
            let dir_id = format!("dir:{}", dirs[..=i].join("/"));
            add(&mut out, TestItem { id: dir_id.clone(), label: dirs[i].to_string(), parent: parent.clone(), path: None, line: None, has_children: true, debuggable: false });
            parent = Some(dir_id);
        }
        let path = root.join(file);
        let text = texts.entry(file.to_string()).or_insert_with(|| std::fs::read_to_string(&path).unwrap_or_default()).clone();
        add(&mut out, TestItem { id: file.to_string(), label: dirs[dirs.len() - 1].to_string(), parent, path: Some(path.clone()), line: Some(0), has_children: true, debuggable: true });
        let mut parent = file.to_string();
        let mut class = None;
        for (i, part) in rest.iter().enumerate() {
            let last = i == rest.len() - 1;
            let node = format!("{parent}::{part}");
            if !last {
                // A class.
                class = Some(*part);
                add(&mut out, TestItem { id: node.clone(), label: part.to_string(), parent: Some(parent.clone()), path: Some(path.clone()), line: line_of(&text, Some(part), None), has_children: true, debuggable: true });
                parent = node;
                continue;
            }
            let (func, param) = match part.find('[') {
                Some(b) => (&part[..b], Some(&part[b..])),
                None => (*part, None),
            };
            let line = line_of(&text, class, Some(func));
            match param {
                Some(param) => {
                    let func_id = format!("{parent}::{func}");
                    add(&mut out, TestItem { id: func_id.clone(), label: func.to_string(), parent: Some(parent.clone()), path: Some(path.clone()), line, has_children: true, debuggable: true });
                    add(&mut out, TestItem { id: id.clone(), label: param.to_string(), parent: Some(func_id), path: Some(path.clone()), line, has_children: false, debuggable: true });
                }
                None => add(&mut out, TestItem { id: id.clone(), label: func.to_string(), parent: Some(parent.clone()), path: Some(path.clone()), line, has_children: false, debuggable: true }),
            }
        }
    }
    out
}

const STATUSES: [(&str, TestState); 6] = [
    ("PASSED", TestState::Passed),
    ("FAILED", TestState::Failed),
    ("ERROR", TestState::Errored),
    ("SKIPPED", TestState::Skipped),
    ("XFAIL", TestState::Skipped),
    ("XPASS", TestState::Passed),
];

/// A `pytest -v` result line: `tests/test_a.py::test_x PASSED   [ 50%]`.
fn result_line(line: &str) -> Option<(String, TestState)> {
    STATUSES.iter().find_map(|(word, state)| {
        let i = line.find(&format!(" {word}"))?;
        let id = &line[..i];
        let after = line[i + 1 + word.len()..].chars().next();
        (id.contains("::") && !id.contains(' ') && after.is_none_or(|c| c == ' ')).then(|| (id.to_string(), *state))
    })
}

/// How pytest names a test in its FAILURES section: `TestC.test_m[1]`.
fn short_name(nodeid: &str) -> String {
    nodeid.split("::").skip(1).collect::<Vec<_>>().join(".")
}

/// The failure reports (`____ test_x ____` and what follows) by name, with the location of
/// the last `file.py:12:` line in each.
fn failure_blocks(lines: &[String], root: &Path) -> HashMap<String, TestMessage> {
    let mut out = HashMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    let mut in_section = false;
    let finish = |current: &mut Option<(String, Vec<&str>)>, out: &mut HashMap<String, TestMessage>| {
        if let Some((name, body)) = current.take() {
            let location = body.iter().rev().find_map(|l| {
                let (file, rest) = l.split_once(':')?;
                let n: usize = rest.split(':').next()?.trim().parse().ok()?;
                (file.ends_with(".py") && !file.contains(' ')).then(|| (root.join(file), n.saturating_sub(1)))
            });
            // The error lines (`E   assert 2 == 3`) first: inline messages show the first line.
            let errors: Vec<&str> = body.iter().filter_map(|l| l.strip_prefix("E ")).map(str::trim).filter(|l| !l.is_empty()).collect();
            let full = body.join("\n").trim().to_string();
            let text = if errors.is_empty() { full } else { format!("{}\n\n{full}", errors.join("\n")) };
            out.insert(name, TestMessage { text, location });
        }
    };
    for line in lines {
        let t = line.trim();
        if t.starts_with("===") {
            finish(&mut current, &mut out);
            in_section = t.contains(" FAILURES ") || t.contains(" ERRORS ");
            continue;
        }
        if !in_section {
            continue;
        }
        if t.starts_with("___") && t.ends_with("___") {
            finish(&mut current, &mut out);
            let name = t.trim_matches('_').trim();
            let name = name.strip_prefix("ERROR at setup of ").or_else(|| name.strip_prefix("ERROR at teardown of ")).unwrap_or(name);
            current = Some((name.to_string(), Vec::new()));
        } else if let Some((_, body)) = &mut current {
            body.push(line);
        }
    }
    finish(&mut current, &mut out);
    out
}

enum Job {
    Collect,
    Run,
}

pub struct PytestTests {
    root: PathBuf,
    python: PathBuf,
    waker: lsp::Waker,
    collected: bool,
    recollect: bool,
    proc: Option<(Job, Proc, Vec<String>)>,
    /// Failed tests of the run, waiting for their messages at its end.
    failed: Vec<String>,
}

impl PytestTests {
    pub fn detect(root: &Path, waker: &lsp::Waker) -> Option<Self> {
        has_tests(root).then(|| PytestTests { root: root.to_path_buf(), python: interpreter(root), waker: waker.clone(), collected: false, recollect: false, proc: None, failed: Vec::new() })
    }

    fn start(&mut self, job: Job, args: Vec<String>) -> Result<(), String> {
        let proc = Proc::spawn(&self.python, &args, &self.root, &[("PYTHONUNBUFFERED", "1")], self.waker.clone()).map_err(|e| format!("Couldn't run {}: {e}", self.python.display()))?;
        self.proc = Some((job, proc, Vec::new()));
        Ok(())
    }

    fn collect(&mut self) {
        if self.proc.is_some() {
            return;
        }
        self.collected = true;
        self.recollect = false;
        let _ = self.start(Job::Collect, pytest_args(&["--collect-only", "-q"]));
    }

    fn collection_done(&mut self, lines: &[String], code: Option<i32>) -> Vec<TestEvent> {
        if lines.iter().any(|l| l.contains("No module named pytest")) {
            return vec![TestEvent::Error(format!("pytest isn't installed for {}", self.python.display()))];
        }
        let ids: Vec<String> = lines.iter().filter(|l| l.contains("::") && !l.starts_with(' ') && !l.contains(' ')).cloned().collect();
        let mut out = vec![TestEvent::Discovered { replace: Replace::TopLevel, items: items(&self.root, &ids) }];
        // 0: collected, 5: nothing to collect; anything else is an error worth showing.
        if !matches!(code, Some(0 | 5)) {
            let why = lines.iter().find(|l| l.starts_with("ERROR ") || l.starts_with("E ")).cloned().unwrap_or_else(|| "pytest couldn't collect the tests".into());
            out.push(TestEvent::Error(why));
        }
        out
    }
}

impl TestProvider for PytestTests {
    fn name(&self) -> &str {
        "pytest"
    }

    fn tick(&mut self, _cx: &mut TestCx, wanted: bool) -> Vec<TestEvent> {
        let mut out = Vec::new();
        if (wanted && !self.collected) || (self.recollect && self.proc.is_none()) {
            self.collect();
        }
        let Some((job, proc, lines)) = &mut self.proc else { return out };
        let mut done = None;
        for line in proc.poll() {
            match line {
                Line::Out(l) => {
                    if matches!(job, Job::Run) {
                        if let Some((id, state)) = result_line(&l) {
                            if state.is_failure() {
                                self.failed.push(id.clone());
                            }
                            out.push(TestEvent::State { id, state, message: None });
                        }
                        out.push(TestEvent::Output(l.clone()));
                    }
                    lines.push(l);
                }
                Line::Done(code) => done = Some(code),
            }
        }
        if let Some(code) = done {
            let (job, _, lines) = self.proc.take().unwrap();
            match job {
                Job::Collect => out.extend(self.collection_done(&lines, code)),
                Job::Run => {
                    let blocks = failure_blocks(&lines, &self.root);
                    for id in std::mem::take(&mut self.failed) {
                        if let Some(message) = blocks.get(&short_name(&id)) {
                            let state = if lines.iter().any(|l| l.starts_with(&format!("{id} ERROR"))) { TestState::Errored } else { TestState::Failed };
                            out.push(TestEvent::State { id, state, message: Some(message.clone()) });
                        }
                    }
                    if lines.iter().any(|l| l.contains("No module named pytest")) {
                        out.push(TestEvent::Error(format!("pytest isn't installed for {}", self.python.display())));
                    }
                    out.push(TestEvent::RunEnded);
                }
            }
        }
        out
    }

    fn discover(&mut self, _cx: &mut TestCx, _parent: Option<&str>) {
        self.recollect = true;
    }

    fn run(&mut self, _cx: &mut TestCx, include: &[String]) -> Result<(), String> {
        if self.proc.is_some() {
            return Err("pytest is busy.".into());
        }
        self.failed.clear();
        // Folders by their path, everything else by node id.
        let targets: Vec<String> = include.iter().map(|id| id.strip_prefix("dir:").unwrap_or(id).to_string()).collect();
        let mut args = pytest_args(&["-v", "--tb=short"]);
        args.extend(targets);
        self.start(Job::Run, args)
    }

    fn cancel(&mut self, _cx: &mut TestCx) {
        if let Some((_, proc, _)) = self.proc.take() {
            proc.kill();
        }
    }

    fn debug(&self, id: &str) -> Option<DebugPlan> {
        let target = id.strip_prefix("dir:").unwrap_or(id);
        let config = json!({
            "type": "debugpy", "request": "launch", "name": format!("Debug {target}"),
            "module": "pytest", "args": [target, "-p", "no:cacheprovider"],
            "cwd": self.root, "python": self.python, "console": "internalConsole", "justMyCode": true,
        });
        Some(runnables::direct(target.to_string(), self.root.clone(), config))
    }

    fn files_changed(&mut self, paths: &[PathBuf]) {
        let relevant = |p: &PathBuf| {
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (n.ends_with(".py") && (n.starts_with("test_") || n.ends_with("_test.py") || n == "conftest.py")) || matches!(n.as_str(), "pytest.ini" | "pyproject.toml" | "setup.cfg" | "tox.ini")
        };
        if self.collected && paths.iter().any(|p| p.starts_with(&self.root) && !p.components().any(|c| c.as_os_str() == ".venv") && relevant(p)) {
            self.recollect = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_items_from_node_ids() {
        let ids = ["tests/test_a.py::test_x".to_string(), "tests/test_a.py::TestC::test_m".to_string(), "tests/test_a.py::test_p[1-2]".to_string()];
        let items = items(Path::new("/nowhere"), &ids);
        let pairs: Vec<(&str, Option<&str>)> = items.iter().map(|i| (i.id.as_str(), i.parent.as_deref())).collect();
        assert_eq!(
            pairs,
            [
                ("dir:tests", None),
                ("tests/test_a.py", Some("dir:tests")),
                ("tests/test_a.py::test_x", Some("tests/test_a.py")),
                ("tests/test_a.py::TestC", Some("tests/test_a.py")),
                ("tests/test_a.py::TestC::test_m", Some("tests/test_a.py::TestC")),
                ("tests/test_a.py::test_p", Some("tests/test_a.py")),
                ("tests/test_a.py::test_p[1-2]", Some("tests/test_a.py::test_p")),
            ]
        );
        assert_eq!(line_of("import x\n\nclass TestC:\n    def test_m(self):\n        pass\n\nasync def test_x():\n", Some("TestC"), Some("test_m")), Some(3));
        assert_eq!(line_of("async def test_x():\n", None, Some("test_x")), Some(0));
    }

    #[test]
    fn reads_results_and_failures() {
        assert_eq!(result_line("tests/test_a.py::test_x PASSED                [ 50%]"), Some(("tests/test_a.py::test_x".into(), TestState::Passed)));
        assert_eq!(result_line("t.py::test_s SKIPPED (no db)  [100%]"), Some(("t.py::test_s".into(), TestState::Skipped)));
        assert_eq!(result_line("FAILED t.py::test_y - assert 1 == 2"), None);
        let out: Vec<String> = "=================================== FAILURES ===================================\n___________________________________ TestC.test_m ___________________________________\ntests/test_a.py:9: in test_m\n    assert 1 == 2\nE   assert 1 == 2\n=========================== short test summary info ============================\nFAILED tests/test_a.py::TestC::test_m - assert 1 == 2".lines().map(String::from).collect();
        let blocks = failure_blocks(&out, Path::new("/p"));
        let m = &blocks[&short_name("tests/test_a.py::TestC::test_m")];
        assert!(m.text.starts_with("assert 1 == 2\n\n") && m.text.ends_with("E   assert 1 == 2"));
        assert_eq!(m.location, Some((PathBuf::from("/p/tests/test_a.py"), 8)));
    }

    /// Needs a Python with pytest: `ORBVANE_TEST_PYTHON=/path/to/venv/bin/python`.
    #[test]
    fn runs_real_pytest() {
        use crate::testing::drive;
        let Some(python) = std::env::var_os("ORBVANE_TEST_PYTHON") else { return eprintln!("ORBVANE_TEST_PYTHON not set; skipped") };
        let root = drive::scratch("pytest");
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("tests/test_math.py"),
            "import pytest\n\n\ndef test_ok():\n    assert 1 + 1 == 2\n\n\nclass TestThings:\n    def test_bad(self):\n        assert 2 * 2 == 5\n\n\n@pytest.mark.parametrize('n', [1, 2])\ndef test_even(n):\n    assert n % 2 == 0\n",
        )
        .unwrap();
        let waker: lsp::Waker = std::sync::Arc::new(|| {});
        let mut p = PytestTests::detect(&root, &waker).unwrap();
        p.python = PathBuf::from(python);
        let found = drive::until(&mut p, &root, |e| e.iter().any(|e| matches!(e, TestEvent::Discovered { .. })));
        let Some(TestEvent::Discovered { items, .. }) = found.iter().find(|e| matches!(e, TestEvent::Discovered { .. })) else { panic!() };
        let ids: Vec<(&str, Option<usize>)> = items.iter().map(|i| (i.id.as_str(), i.line)).collect();
        assert!(ids.contains(&("tests/test_math.py::TestThings::test_bad", Some(8))), "{ids:?}");
        assert!(ids.contains(&("tests/test_math.py::test_even[2]", Some(13))), "{ids:?}");

        let events = drive::run(&mut p, &root, &[]);
        assert_eq!(drive::state(&events, "tests/test_math.py::test_ok").unwrap().0, TestState::Passed);
        assert_eq!(drive::state(&events, "tests/test_math.py::test_even[2]").unwrap().0, TestState::Passed);
        assert_eq!(drive::state(&events, "tests/test_math.py::test_even[1]").unwrap().0, TestState::Failed);
        let (state, message) = drive::state(&events, "tests/test_math.py::TestThings::test_bad").unwrap();
        assert_eq!(state, TestState::Failed);
        let message = message.unwrap();
        assert!(message.text.contains("assert 2 * 2 == 5"), "{}", message.text);
        assert_eq!(message.location, Some((root.join("tests/test_math.py"), 9)));
    }
}

