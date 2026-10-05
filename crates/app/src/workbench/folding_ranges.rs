//! Folding regions from the language server (`textDocument/foldingRange`), used instead of
//! indentation when `editor.foldingStrategy` is `auto`. Fetched for the
//! documents on screen after typing pauses; until the next answer, regions move with edits.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Workbench;
use crate::folding::{FoldRange, ServerRanges};

const DELAY: Duration = Duration::from_millis(400);
const RETRY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct FoldingState {
    docs: HashMap<usize, DocRanges>,
    generation: u64,
    work_done: u64,
    auto: bool,
}

impl FoldingState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
    }
}

#[derive(Default)]
struct DocRanges {
    ranges: Option<Vec<FoldRange>>,
    seq: Option<u64>,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    unsupported: bool,
    dirty: bool,
}

/// Sorted by start, one region per start line (the outermost), as `Folds` expects.
fn normalize(mut ranges: Vec<FoldRange>) -> Vec<FoldRange> {
    ranges.retain(|r| r.end > r.start);
    ranges.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    ranges.dedup_by_key(|r| r.start);
    ranges
}

/// Moves regions with the edits since `seq`: regions below an edit shift, regions around it
/// grow or shrink, regions starting on edited lines are dropped.
fn shift(ranges: &mut Vec<FoldRange>, b: &text::Buffer, seq: u64) -> bool {
    let Some(edits) = b.edits_since(seq) else { return false };
    for change in edits {
        let text::Change::Edit(e) = change else { return false };
        let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
        let delta = new_end as isize - old_end as isize;
        let moved = |l: usize| (l as isize + delta).max(0) as usize;
        ranges.retain_mut(|r| {
            if r.start > old_end {
                (r.start, r.end) = (moved(r.start), moved(r.end));
                true
            } else if r.start > start {
                false
            } else {
                if r.end >= start {
                    r.end = moved(r.end.max(old_end)).max(r.start);
                }
                true
            }
        });
    }
    true
}

impl Workbench {
    /// Follows edits and asks for regions after a pause. Called every frame.
    pub(super) fn folding_tick(&mut self) {
        let now = Instant::now();
        let auto = self.settings.string("editor.foldingStrategy") != "indentation";
        if auto != self.folding.auto {
            self.folding.auto = auto;
            for st in self.folding.docs.values_mut() {
                st.dirty = true;
                st.due = Some(now);
            }
        }
        if self.folding.work_done != self.lsp.work_done {
            self.folding.work_done = self.lsp.work_done;
            for st in self.folding.docs.values_mut() {
                st.due = Some(now);
                st.unsupported = false;
            }
        }
        let on_screen = self.on_screen_docs();
        self.folding.docs.retain(|d, _| on_screen.iter().any(|(v, _)| v == d));
        for (doc_id, path) in on_screen {
            let Some(doc) = self.docs[doc_id].as_ref() else { continue };
            let st = self.folding.docs.entry(doc_id).or_default();
            let seq = doc.buffer.edit_seq();
            match st.seq {
                None => st.due = Some(now),
                Some(old) if old != seq => {
                    if let Some(r) = &mut st.ranges {
                        if !shift(r, &doc.buffer, old) {
                            st.ranges = None;
                        }
                        st.dirty = true;
                    }
                    st.due = Some(now + DELAY);
                }
                _ => {}
            }
            st.seq = Some(seq);
            if st.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
                st.in_flight = None;
            }
            if !auto || st.unsupported || st.in_flight.is_some() || !st.due.is_some_and(|t| now >= t) {
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
            let asked = self.lsp.folding_ranges(&path, &doc.buffer);
            let st = self.folding.docs.get_mut(&doc_id).unwrap();
            if asked {
                st.in_flight = Some(now);
            } else {
                st.unsupported = true;
            }
        }
        self.publish_folding();
    }

    fn publish_folding(&mut self) {
        let auto = self.folding.auto;
        for (&doc_id, st) in &mut self.folding.docs {
            if !std::mem::take(&mut st.dirty) {
                continue;
            }
            if let Some(doc) = self.docs[doc_id].as_mut() {
                self.folding.generation += 1;
                let ranges = st.ranges.clone().filter(|_| auto).map(Arc::new);
                doc.folding = ServerRanges { ranges, generation: self.folding.generation };
            }
        }
    }

    pub(super) fn folding_deadline(&self) -> Option<Instant> {
        self.folding.docs.values().filter_map(|st| st.due).min()
    }

    pub(super) fn folding_arrived(&mut self, path: &Path, version: u64, ranges: Option<Vec<(usize, usize)>>) {
        let Some(doc_id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        let Some(st) = self.folding.docs.get_mut(&doc_id) else { return };
        let doc = self.docs[doc_id].as_ref().unwrap();
        st.in_flight = None;
        let Some(ranges) = ranges else {
            st.due = Some(Instant::now() + RETRY);
            return;
        };
        if version != doc.buffer.version() {
            st.due.get_or_insert(Instant::now());
            return;
        }
        // Keep a closing bracket's line visible (`}`), like indentation folding.
        let b = &doc.buffer;
        let closes = |l: usize| l < b.len_lines() && b.line(l).trim_start().starts_with(['}', ']', ')']);
        let ranges = ranges.into_iter().map(|(start, end)| FoldRange { start, end: if closes(end) && end > start + 1 { end - 1 } else { end } });
        st.ranges = Some(normalize(ranges.collect()));
        st.seq = Some(doc.buffer.edit_seq());
        st.dirty = true;
        self.publish_folding();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_region_per_start() {
        let r = normalize(vec![FoldRange { start: 3, end: 5 }, FoldRange { start: 0, end: 9 }, FoldRange { start: 3, end: 8 }, FoldRange { start: 4, end: 4 }]);
        assert_eq!(r, vec![FoldRange { start: 0, end: 9 }, FoldRange { start: 3, end: 8 }]);
    }
}
