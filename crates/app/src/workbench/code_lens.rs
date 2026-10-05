//! Code lenses: the small actions the
//! language server puts above lines, like rust-analyzer's "▶ Run | Debug" over tests and
//! "3 implementations" over types. They're fetched for the documents on screen after typing
//! pauses, move with edits, are resolved when they come without a title, and are handed to
//! the editor (`Doc::lenses`), which draws them in rows of their own above their lines.
//!
//! rust-analyzer's lenses use commands its editor extension implements; we implement them
//! too: Run runs the runnable in a task terminal, Debug builds it and starts lldb-dap on the
//! executable, and references open in the references view. Other commands go to the server.
//! Test providers that ask for it (Go) add lenses of our own after the server's, made from
//! the test tree (`orbvane.testing.run` / `.debug`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp::Encoding;
use serde_json::{json, Value};

use super::Workbench;
use crate::layout::Lenses;
use crate::runnables;

/// How long typing pauses before lenses are asked for again.
const DELAY: Duration = Duration::from_millis(600);
/// When a server couldn't answer (still loading), ask again after this long.
const RETRY: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct LensState {
    docs: HashMap<usize, DocLenses>,
    work_done: u64,
    generation: u64,
}

impl LensState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
    }
}

#[derive(Default)]
struct DocLenses {
    /// The server's lenses, the line each is on now (moved with edits), and the version of
    /// the buffer they were asked for.
    raw: Vec<Value>,
    lines: Vec<usize>,
    version: u64,
    encoding: Option<Encoding>,
    seq: Option<u64>,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    unsupported: bool,
    /// Lenses being resolved.
    resolving: HashSet<usize>,
    dirty: bool,
    /// Our own lenses after the server's (a test provider's "run test | debug test"), the
    /// line each is on now, and the test tree version they were made for.
    extra: Vec<Value>,
    extra_lines: Vec<usize>,
    tests_version: Option<u64>,
}

/// Moves lens lines with the edits since `seq` (lines inside an edit stay put).
fn shift(lines: &mut [usize], b: &text::Buffer, seq: u64) {
    let Some(edits) = b.edits_since(seq) else { return };
    for change in edits {
        let text::Change::Edit(e) = change else { continue };
        let (old_end, new_end) = (e.old_end.0, e.new_end.0);
        let delta = new_end as isize - old_end as isize;
        for l in lines.iter_mut().filter(|l| **l > old_end) {
            *l = (*l as isize + delta).max(0) as usize;
        }
    }
}

impl Workbench {
    /// Follows edits and asks for lenses after a pause. Called every frame.
    pub(super) fn lens_tick(&mut self) {
        let now = Instant::now();
        let enabled = self.settings.bool("editor.codeLens");
        if std::mem::take(&mut self.lsp.lens_refresh) || self.lenses.work_done != self.lsp.work_done {
            self.lenses.work_done = self.lsp.work_done;
            for st in self.lenses.docs.values_mut() {
                st.due = Some(now);
                st.unsupported = false;
            }
        }
        let on_screen = if enabled { self.on_screen_docs() } else { Vec::new() };
        let gone: Vec<usize> = self.lenses.docs.keys().copied().filter(|d| !on_screen.iter().any(|(v, _)| v == d)).collect();
        for d in gone {
            self.lenses.docs.remove(&d);
            if let Some(doc) = self.docs.get_mut(d).and_then(Option::as_mut) {
                self.lenses.generation += 1;
                doc.lenses = Lenses { generation: self.lenses.generation, ..Default::default() };
            }
        }
        for (doc_id, path) in on_screen {
            let Some(doc) = self.docs[doc_id].as_ref() else { continue };
            if doc.large {
                continue;
            }
            let st = self.lenses.docs.entry(doc_id).or_default();
            let seq = doc.buffer.edit_seq();
            match st.seq {
                None => st.due = Some(now),
                Some(old) if old != seq => {
                    shift(&mut st.lines, &doc.buffer, old);
                    shift(&mut st.extra_lines, &doc.buffer, old);
                    st.dirty = true;
                    st.due = Some(now + DELAY);
                }
                _ => {}
            }
            st.seq = Some(seq);
            if st.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
                st.in_flight = None;
            }
            if st.unsupported || st.in_flight.is_some() || !st.due.is_some_and(|t| now >= t) {
                continue;
            }
            st.due = None;
            if !self.lsp.is_running(&path) {
                if self.lsp.has_server(&path) {
                    st.due = Some(now + RETRY);
                }
                continue;
            }
            let doc = self.docs[doc_id].as_ref().unwrap();
            let asked = self.lsp.code_lenses(&path, &doc.buffer);
            let st = self.lenses.docs.get_mut(&doc_id).unwrap();
            if asked {
                st.in_flight = Some(now);
            } else {
                st.unsupported = true;
            }
        }
        self.refresh_test_lenses();
        self.resolve_lenses();
        self.publish_lenses();
    }

    /// Test providers that want them (Go) get lenses: "run package tests | run
    /// file tests" on a test file's package line, "run test | debug test" over each test.
    fn refresh_test_lenses(&mut self) {
        let version = self.testing.tree.version;
        let docs: Vec<usize> = self.lenses.docs.iter().filter(|(_, st)| st.tests_version != Some(version)).map(|(&d, _)| d).collect();
        for doc_id in docs {
            let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) else { continue };
            let tree = &self.testing.tree;
            let mut lenses: Vec<(usize, &str, &str, usize, String)> = Vec::new();
            for (key, item) in tree.in_file(&path) {
                if !self.testing.providers.get(key.0).is_some_and(|p| p.code_lenses()) {
                    continue;
                }
                let Some(line) = item.line else { continue };
                let parent = item.parent.as_ref().and_then(|id| tree.item(&(key.0, id.clone())));
                match parent {
                    // A file in its package.
                    Some(pkg) if pkg.path.is_none() => {
                        lenses.push((line, "run package tests", "orbvane.testing.run", key.0, pkg.id.clone()));
                        lenses.push((line, "run file tests", "orbvane.testing.run", key.0, item.id.clone()));
                    }
                    // A subtest sits on its test's line.
                    Some(p) if p.line == Some(line) => {}
                    _ => {
                        let bench = item.label.starts_with("Benchmark");
                        lenses.push((line, if bench { "run benchmark" } else { "run test" }, "orbvane.testing.run", key.0, item.id.clone()));
                        lenses.push((line, if bench { "debug benchmark" } else { "debug test" }, "orbvane.testing.debug", key.0, item.id.clone()));
                    }
                }
            }
            // One pair per line (repeated subtests share their `t.Run` line).
            lenses.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
            lenses.dedup_by(|a, b| (a.0, a.1) == (b.0, b.1));
            lenses.sort_by_key(|l| (l.0, !l.1.starts_with("run"), !l.1.contains("package")));
            let lines = lenses.iter().map(|l| l.0).collect();
            let extra = lenses
                .into_iter()
                .map(|(line, title, command, p, id)| {
                    json!({
                        "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 0 } },
                        "command": { "title": title, "command": command, "arguments": [p, id] },
                    })
                })
                .collect();
            let st = self.lenses.docs.get_mut(&doc_id).unwrap();
            st.extra = extra;
            st.extra_lines = lines;
            st.tests_version = Some(version);
            st.dirty = true;
        }
    }

    /// Resolves the lenses that came without a command (rust-analyzer sends the reference
    /// and implementation counts that way).
    fn resolve_lenses(&mut self) {
        let mut asks = Vec::new();
        for (&doc_id, st) in &self.lenses.docs {
            let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()) else { continue };
            for (i, lens) in st.raw.iter().enumerate() {
                if lens.get("command").is_none_or(Value::is_null) && !st.resolving.contains(&i) {
                    asks.push((doc_id, path.to_path_buf(), st.version, i, lens.clone()));
                }
            }
        }
        for (doc_id, path, version, i, lens) in asks {
            let asked = self.lsp.resolve_code_lens(&path, version, i, lens);
            if let Some(st) = self.lenses.docs.get_mut(&doc_id) {
                st.resolving.insert(i);
                if !asked {
                    // No resolve: nothing to show for it.
                    st.raw[i]["command"] = json!({ "title": "", "command": "" });
                }
            }
        }
    }

    fn publish_lenses(&mut self) {
        for (&doc_id, st) in &mut self.lenses.docs {
            if !st.dirty {
                continue;
            }
            st.dirty = false;
            let Some(doc) = self.docs[doc_id].as_mut() else { continue };
            let n = doc.buffer.len_lines();
            let items: Vec<(usize, Option<String>, usize)> = st
                .raw
                .iter()
                .zip(&st.lines)
                .chain(st.extra.iter().zip(&st.extra_lines))
                .enumerate()
                .filter(|(_, (_, line))| **line < n)
                .map(|(i, (lens, &line))| (line, lens["command"]["title"].as_str().map(|t| t.to_string()), i))
                .filter(|(_, title, _)| title.as_deref() != Some(""))
                .collect();
            let mut lines: Vec<usize> = items.iter().map(|i| i.0).collect();
            lines.sort_unstable();
            lines.dedup();
            self.lenses.generation += 1;
            doc.lenses = Lenses { lines: Arc::new(lines), items: Arc::new(items), generation: self.lenses.generation };
        }
    }

    pub(super) fn lens_deadline(&self) -> Option<Instant> {
        self.lenses.docs.values().filter_map(|st| st.due).min()
    }

    /// A server's lenses for `path` at buffer `version` (None: ask again soon).
    pub(super) fn code_lenses_arrived(&mut self, path: &Path, version: u64, lenses: Option<Vec<Value>>, encoding: Encoding) {
        let Some(doc_id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        let Some(st) = self.lenses.docs.get_mut(&doc_id) else { return };
        let doc = self.docs[doc_id].as_ref().unwrap();
        st.in_flight = None;
        let Some(lenses) = lenses else {
            st.due = Some(Instant::now() + RETRY);
            return;
        };
        if version != doc.buffer.version() {
            st.due.get_or_insert(Instant::now());
            return;
        }
        st.lines = lenses.iter().map(|l| l["range"]["start"]["line"].as_u64().unwrap_or(0) as usize).collect();
        st.raw = lenses;
        st.version = version;
        st.encoding = Some(encoding);
        st.resolving.clear();
        st.seq = Some(doc.buffer.edit_seq());
        st.dirty = true;
    }

    pub(super) fn code_lens_resolved(&mut self, path: &Path, version: u64, index: usize, lens: Value) {
        let Some(doc_id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        let Some(st) = self.lenses.docs.get_mut(&doc_id) else { return };
        if st.version != version || index >= st.raw.len() {
            return;
        }
        st.raw[index] = lens;
        st.dirty = true;
    }

    /// A click on lens `index` of document `doc_id`.
    pub(super) fn run_code_lens(&mut self, doc_id: usize, index: usize) {
        let Some(st) = self.lenses.docs.get(&doc_id) else { return };
        let lens = st.raw.get(index).or_else(|| st.extra.get(index.checked_sub(st.raw.len())?));
        let Some(command) = lens.map(|l| l["command"].clone()) else { return };
        let encoding = st.encoding.unwrap_or(Encoding::Utf16);
        let args = command["arguments"].as_array().cloned().unwrap_or_default();
        match command["command"].as_str().unwrap_or_default() {
            "rust-analyzer.runSingle" => {
                let Some(runnable) = args.first() else { return };
                let Some((line, dir)) = runnables::command(runnable) else { return };
                let label = runnable["label"].as_str().unwrap_or("run").to_string();
                if let Err(e) = self.run_task_terminal(&label, &line, &dir, &runnables::environment(runnable)) {
                    self.set_status_message(&e);
                }
            }
            "rust-analyzer.debugSingle" => {
                if let Some(plan) = args.first().and_then(runnables::debug_plan) {
                    self.start_debug_plan(plan);
                }
            }
            "rust-analyzer.showReferences" => {
                let locations = args.get(2).map(lsp::parse_locations).unwrap_or_default();
                self.show_references(locations, encoding);
            }
            "orbvane.testing.run" | "orbvane.testing.debug" => {
                let (Some(p), Some(id)) = (args.first().and_then(Value::as_u64), args.get(1).and_then(Value::as_str)) else { return };
                let key = (p as usize, id.to_string());
                if command["command"] == "orbvane.testing.run" {
                    self.run_tests(vec![key]);
                } else {
                    self.debug_test(&key);
                }
            }
            "rust-analyzer.gotoLocation" => {
                if let Some(loc) = args.first().and_then(|a| lsp::parse_locations(&Value::Array(vec![a.clone()])).pop()) {
                    let line = loc.range.start.line as usize;
                    self.goto_location(&loc.path, text::Pos::new(line, 0));
                }
            }
            "" => {}
            _ => {
                let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) else { return };
                if let Some(key) = self.lsp.key_for(&path) {
                    if !self.lsp.execute_command(&key, &command) {
                        self.set_status_message("The language server can't run this command.");
                    }
                }
            }
        }
    }
}
