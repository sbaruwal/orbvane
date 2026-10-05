//! Inlay hints: the types and parameter names the language server shows inside the code
//! (`textDocument/inlayHint`), like `editor.inlayHints`. Hints are fetched for the
//! documents on screen after typing pauses, move with edits until the next answer (hints on
//! edited lines are dropped), and are handed to the editor through `Doc::inlays`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp::Encoding;

use super::Workbench;
use crate::layout::{InlayHint, Inlays};

/// How long typing pauses before hints are asked for again.
const DELAY: Duration = Duration::from_millis(400);
/// When a server couldn't answer (still loading), ask again after this long.
const RETRY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct InlayState {
    docs: HashMap<usize, DocHints>,
    /// Control+Option held (the `...UnlessPressed` modes flip visibility while it is).
    pub(super) ctrl_alt: bool,
    shown: bool,
    work_done: u64,
    generation: u64,
}

impl InlayState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
    }
}

#[derive(Default)]
struct DocHints {
    /// The latest hints, by line, and the edit sequence they've been moved to.
    hints: HashMap<usize, Vec<InlayHint>>,
    seq: Option<u64>,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    unsupported: bool,
    /// `hints` changed since they were handed to the document.
    dirty: bool,
}

/// Moves hints with the edits since `seq`: lines after an edit shift, edited lines lose theirs.
pub(super) fn shift(hints: &mut HashMap<usize, Vec<InlayHint>>, b: &text::Buffer, seq: u64) {
    let Some(edits) = b.edits_since(seq) else {
        hints.clear();
        return;
    };
    for change in edits {
        let text::Change::Edit(e) = change else {
            hints.clear();
            return;
        };
        let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
        let delta = new_end as isize - old_end as isize;
        *hints = std::mem::take(hints)
            .into_iter()
            .filter(|(line, _)| *line < start || *line > old_end)
            .map(|(line, h)| (if line > old_end { (line as isize + delta) as usize } else { line }, h))
            .collect();
    }
}

impl Workbench {
    fn inlays_visible(&self) -> bool {
        let held = self.inlays.ctrl_alt;
        match self.settings.string("editor.inlayHints.enabled").as_str() {
            "off" => false,
            "offUnlessPressed" => held,
            "onUnlessPressed" => !held,
            _ => true,
        }
    }

    /// Follows edits and asks for hints after a pause. Called every frame.
    pub(super) fn inlay_tick(&mut self) {
        let now = Instant::now();
        let visible = self.inlays_visible();
        if std::mem::take(&mut self.lsp.inlay_refresh) || self.inlays.work_done != self.lsp.work_done {
            self.inlays.work_done = self.lsp.work_done;
            for st in self.inlays.docs.values_mut() {
                st.due = Some(now);
                st.unsupported = false;
            }
        }
        let on_screen = self.on_screen_docs();
        // Documents no longer on screen drop their hints (they'd go stale).
        let gone: Vec<usize> = self.inlays.docs.keys().copied().filter(|d| !on_screen.iter().any(|(v, _)| v == d)).collect();
        for d in gone {
            self.inlays.docs.remove(&d);
            if let Some(doc) = self.docs.get_mut(d).and_then(Option::as_mut) {
                self.inlays.generation += 1;
                doc.inlays = Inlays { lines: Arc::default(), generation: self.inlays.generation };
            }
        }
        for (doc_id, path) in on_screen {
            let Some(doc) = self.docs[doc_id].as_ref() else { continue };
            let st = self.inlays.docs.entry(doc_id).or_default();
            let seq = doc.buffer.edit_seq();
            match st.seq {
                None => st.due = Some(now),
                Some(old) if old != seq => {
                    shift(&mut st.hints, &doc.buffer, old);
                    st.dirty = true;
                    st.due = Some(now + DELAY);
                }
                _ => {}
            }
            st.seq = Some(seq);
            if st.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
                st.in_flight = None;
            }
            if st.unsupported || st.in_flight.is_some() || !st.due.is_some_and(|t| now >= t) || !visible {
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
            let lines = 0..doc.buffer.len_lines();
            if self.lsp.inlay_hints(&path, &doc.buffer, lines) {
                self.inlays.docs.get_mut(&doc_id).unwrap().in_flight = Some(now);
            } else {
                self.inlays.docs.get_mut(&doc_id).unwrap().unsupported = true;
            }
        }
        self.publish_inlays(visible);
    }

    /// Hands the current hints to the documents (none while hidden).
    fn publish_inlays(&mut self, visible: bool) {
        let changed = visible != self.inlays.shown;
        self.inlays.shown = visible;
        for (&doc_id, st) in &mut self.inlays.docs {
            let Some(doc) = self.docs[doc_id].as_mut() else { continue };
            if changed || st.dirty {
                st.dirty = false;
                let want = if visible { st.hints.clone() } else { HashMap::new() };
                self.inlays.generation += 1;
                doc.inlays = Inlays { lines: Arc::new(want), generation: self.inlays.generation };
            }
        }
    }

    pub(super) fn inlay_deadline(&self) -> Option<Instant> {
        self.inlays.docs.values().filter_map(|st| st.due).min()
    }

    /// A server's hints for `path` at buffer `version` (None: ask again soon).
    pub(super) fn inlay_hints_arrived(&mut self, path: &Path, version: u64, hints: Option<Vec<lsp::InlayHint>>, encoding: Encoding) {
        let Some(doc_id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        let Some(st) = self.inlays.docs.get_mut(&doc_id) else { return };
        let doc = self.docs[doc_id].as_ref().unwrap();
        st.in_flight = None;
        let Some(hints) = hints else {
            st.due = Some(Instant::now() + RETRY);
            return;
        };
        if version != doc.buffer.version() {
            st.due.get_or_insert(Instant::now()); // edited meanwhile: ask again
            return;
        }
        let mut by_line: HashMap<usize, Vec<InlayHint>> = HashMap::new();
        for h in hints {
            let line = h.position.line as usize;
            if line >= doc.buffer.len_lines() {
                continue;
            }
            let col = encoding.from_lsp(&doc.buffer.line(line), h.position.character);
            let label = h.label.trim().to_string();
            by_line.entry(line).or_default().push(InlayHint { col, label, parameter: h.kind == 2, pad_left: h.padding_left, pad_right: h.padding_right, swatch: None, color: None });
        }
        for hs in by_line.values_mut() {
            hs.sort_by_key(|h| h.col);
        }
        st.hints = by_line;
        st.dirty = true;
        st.seq = Some(doc.buffer.edit_seq());
        let visible = self.inlays_visible();
        self.publish_inlays(visible);
    }
}
