//! The merge editor: a conflicted file's incoming and current versions side
//! by side above, and the result below. The result is the file itself in an ordinary,
//! editable editor tab (`EditorState::merge` is set); each range the two sides changed gets
//! a checkbox and actions (Accept Incoming, Accept Combination, Ignore...) in the inputs and
//! a summary row (Incoming + Current | Remove Incoming...) in the result. Complete Merge saves
//! the file and stages it. The model is `crate::merge`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use language::{Highlighter, Lang, Span};
use render::{Canvas, Color, Rect, TextStyle};
use text::{Buffer, EditKind, Pos, Selection};

use super::{Hit, Workbench, UI};
use crate::editor::{expand_tabs_spans, font_size, line_height, Doc, EditorState, MergeMark};
use crate::icons;
use crate::layout::Lenses;
use crate::merge::{Merge, Region, State};

/// Height of the panes' title bars.
pub(super) const HEADER_H: f32 = 28.0;
const CHECKBOX: f32 = 14.0;

/// Code lens generations for merge editors, apart from the language servers' ones.
static LENS_GENERATION: AtomicU64 = AtomicU64::new(1 << 48);

/// Something an action or checkbox does to a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeAction {
    /// Put the range in this state.
    Set(usize, State),
    /// Don't take this input (1 or 2) for the range.
    Ignore(usize, u8),
}

/// One side of the merge, read-only.
struct Input {
    title: &'static str,
    /// The commit ("abc1234").
    description: String,
    /// The refs pointing at it ("main, origin/main").
    detail: String,
    buffer: Buffer,
    highlight: Highlighter,
}

impl Input {
    fn new(title: &'static str, text: &str, lang: Lang, commit: Option<(String, String)>) -> Self {
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), text);
        let mut highlight = Highlighter::new(lang);
        highlight.update(&mut buffer);
        let (description, detail) = match commit {
            Some((hash, refs)) => {
                let refs: Vec<&str> = refs.split(", ").map(|r| r.trim_start_matches("HEAD -> ")).filter(|r| !r.is_empty() && *r != "HEAD").collect();
                (hash[..hash.len().min(7)].to_string(), refs.join(", "))
            }
            None => (String::new(), String::new()),
        };
        Self { title, description, detail, buffer, highlight }
    }
}

/// A row of an input pane: a line, or the actions above a conflict.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Line(usize),
    Actions(usize),
}

pub struct MergeState {
    pub path: PathBuf,
    model: Merge,
    /// Input 1 (incoming, left) and input 2 (current, right).
    inputs: [Input; 2],
    /// For each range, whether each input has been dealt with.
    handled: Vec<[bool; 2]>,
    regions: Vec<Region>,
    regions_version: Option<u64>,
    /// The result's rows above conflicts, drawn by the editor as code lenses.
    pub lenses: Lenses,
    lens_actions: Vec<MergeAction>,
    /// Drawn last frame, for clicks.
    input_actions: Vec<(Rect, MergeAction)>,
    input_scroll: [f32; 2],
    opened: std::time::Instant,
}

impl MergeState {
    fn input_lines(&self, k: usize) -> usize {
        self.inputs[k].buffer.len_lines()
    }

    /// The range is a conflict (both sides changed it differently).
    fn conflicting(&self, i: usize) -> bool {
        self.model.is_conflicting(i)
    }

    fn handled(&self, i: usize) -> bool {
        self.handled[i] == [true, true]
    }

    pub fn remaining(&self) -> usize {
        (0..self.model.ranges.len()).filter(|&i| self.conflicting(i) && !self.handled(i)).count()
    }

    /// Recomputes what the result holds for each range after it changed.
    fn refresh(&mut self, doc: &Doc) {
        let version = doc.buffer.version();
        if self.regions_version == Some(version) {
            return;
        }
        let regions = self.model.regions(&doc.buffer.text());
        if self.regions_version.is_some() {
            // Editing a conflict by hand deals with it; undoing back to the base doesn't.
            for (i, (old, new)) in self.regions.iter().zip(&regions).enumerate() {
                if old.state != new.state && self.conflicting(i) {
                    self.handled[i] = if new.state == State::Base { [false, false] } else { [true, true] };
                }
            }
        }
        self.regions = regions;
        self.regions_version = Some(version);
        self.rebuild_lenses(doc.buffer.len_lines());
    }

    /// The result's summary rows: what each conflict holds, and how to change it.
    fn rebuild_lenses(&mut self, len_lines: usize) {
        let mut lines = Vec::new();
        let mut items = Vec::new();
        self.lens_actions.clear();
        let title = |k: u8| self.inputs[k as usize - 1].title;
        for (i, reg) in self.regions.iter().enumerate() {
            if !self.model.is_conflicting(i) {
                continue;
            }
            let line = reg.lines.start.min(len_lines.saturating_sub(1));
            lines.push(line);
            let mut label = |text: String, action: Option<MergeAction>| {
                let index = match action {
                    Some(a) => {
                        self.lens_actions.push(a);
                        self.lens_actions.len() - 1
                    }
                    None => usize::MAX,
                };
                items.push((line, Some(text), index));
            };
            match reg.state {
                State::Manual => label("Manual Resolution".into(), None),
                State::Base => label("No Changes Accepted".into(), None),
                State::Input1 => label(title(1).into(), None),
                State::Input2 => label(title(2).into(), None),
                State::Both { first, .. } => label(format!("{} + {}", title(first), title(3 - first)), None),
            }
            let mut removes: Vec<u8> = [1, 2].into_iter().filter(|&k| reg.state.includes(k)).collect();
            if let State::Both { first: 2, .. } = reg.state {
                removes.reverse();
            }
            for k in removes {
                label(format!("Remove {}", title(k)), Some(MergeAction::Set(i, reg.state.with(k, false))));
            }
            if reg.state == State::Manual {
                label("Reset to base".into(), Some(MergeAction::Set(i, State::Base)));
            }
        }
        lines.dedup();
        let generation = LENS_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.lenses = Lenses { lines: Arc::new(lines), items: Arc::new(items), generation };
    }

    /// The actions above range `i` in input `k` (0 or 1), as we offer them.
    fn input_actions_for(&self, i: usize, k: usize) -> Vec<(String, MergeAction)> {
        let input = k as u8 + 1;
        let other = 3 - input;
        let title = self.inputs[k].title;
        let state = self.regions.get(i).map_or(State::Base, |r| r.state);
        let mut out = Vec::new();
        if state == State::Manual || state.includes(input) {
            return out;
        }
        let combine = self.model.can_be_combined(i);
        if !state.includes(other) {
            out.push((format!("Accept {title}"), MergeAction::Set(i, state.with(input, true))));
            if combine {
                let name = if self.model.is_order_relevant(i) { format!("Accept Combination ({title} First)") } else { "Accept Combination".to_string() };
                out.push((name, MergeAction::Set(i, State::Both { first: input, smart: true })));
            }
        } else {
            out.push((format!("Append {title}"), MergeAction::Set(i, state.with(input, true))));
            if combine {
                out.push(("Accept Combination".to_string(), MergeAction::Set(i, State::Both { first: other, smart: true })));
            }
        }
        if !self.handled[i][k] {
            out.push(("Ignore".to_string(), MergeAction::Ignore(i, input)));
        }
        out
    }

    /// Input `k`'s rows: its lines, with an actions row above each conflict.
    fn input_rows(&self, k: usize) -> Vec<Row> {
        let n = self.input_lines(k);
        let mut zones: Vec<(usize, usize)> = (0..self.model.ranges.len())
            .filter(|&i| self.conflicting(i))
            .map(|i| (self.model.ranges[i].input(k as u8 + 1).start.min(n.saturating_sub(1)), i))
            .collect();
        zones.sort_unstable();
        let mut rows = Vec::with_capacity(n + zones.len());
        let mut z = 0;
        for line in 0..n {
            while z < zones.len() && zones[z].0 == line {
                rows.push(Row::Actions(zones[z].1));
                z += 1;
            }
            rows.push(Row::Line(line));
        }
        rows
    }

    /// The line of input `k` that lines up with result line `line`.
    fn input_line_for(&self, k: usize, line: usize) -> usize {
        let input = k as u8 + 1;
        let mut mapped = line;
        for (i, reg) in self.regions.iter().enumerate() {
            let r = self.model.ranges[i].input(input);
            if reg.lines.start > line {
                break;
            }
            mapped = if line < reg.lines.end { r.start + (line - reg.lines.start).min(r.len().saturating_sub(1)) } else { r.end + (line - reg.lines.end) };
        }
        mapped.min(self.input_lines(k).saturating_sub(1))
    }
}

/// The edit that puts `new` in place of result lines `s..e`.
fn splice(b: &Buffer, s: usize, e: usize, new: &[String]) -> (Pos, Pos, String) {
    let n = b.len_lines();
    if e < n {
        (Pos::new(s, 0), Pos::new(e, 0), new.iter().map(|l| format!("{l}\n")).collect())
    } else if s < n {
        if new.is_empty() && s > 0 {
            (Pos::new(s - 1, b.line_len(s - 1)), b.end(), String::new())
        } else {
            (Pos::new(s, 0), b.end(), new.join("\n"))
        }
    } else {
        (b.end(), b.end(), format!("\n{}", new.join("\n")))
    }
}

impl Workbench {
    /// The active group's merge editor: (group, tab).
    fn merge_tab(&self) -> Option<(usize, usize)> {
        let g = self.active_group;
        let gr = self.groups.get(g)?;
        gr.tabs.get(gr.active).filter(|t| t.merge.is_some()).map(|_| (g, gr.active))
    }

    /// Git: Resolve in Merge Editor, for the active editor's file.
    pub(super) fn open_merge_editor_for_active(&mut self) {
        match self.active_editor().and_then(|e| self.docs[e.doc].as_ref()).and_then(|d| d.buffer.path().map(Path::to_path_buf)) {
            Some(path) => self.open_merge_editor(&path),
            None => self.set_status_message("Open a file with merge conflicts first."),
        }
    }

    /// Opens a conflicted file in the merge editor. Its conflict markers are replaced with
    /// the automatic merge (conflicts start as the base).
    pub(super) fn open_merge_editor(&mut self, path: &Path) {
        let Some(root) = self.repo.as_ref().map(|r| r.root.clone()) else { return };
        let g = self.active_group;
        if let Some(i) = self.groups[g].tabs.iter().position(|t| t.merge.as_ref().is_some_and(|m| m.path == path)) {
            self.groups[g].active = i;
            self.focus = super::Focus::Editor;
            return;
        }
        let (Some(current), Some(incoming)) = (scm::show(&root, ":2", path), scm::show(&root, ":3", path)) else {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            self.set_status_message(&format!("{name} has no conflicting versions to merge (one side deleted it, or it isn't in conflict)."));
            return;
        };
        let base = scm::show(&root, ":1", path).unwrap_or_default();
        let norm = |s: String| s.replace("\r\n", "\n");
        let (base, current, incoming) = (norm(base), norm(current), norm(incoming));
        let Some(doc_id) = self.doc_for_path(path) else { return };
        let lang = language::Lang::detect(Some(path));
        let git_dir = scm::git_dir(&root).unwrap_or_else(|| root.join(".git"));
        let rebasing = git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
        let head = scm::commit_refs(&root, "HEAD");
        let other = ["MERGE_HEAD", "REBASE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"].iter().find(|r| git_dir.join(r).exists()).and_then(|r| scm::commit_refs(&root, r));
        // While rebasing, "ours" is the branch being rebased onto; the sides swap.
        let (in1, in2) = if rebasing {
            (Input::new("Current", &current, lang, head), Input::new("Incoming", &incoming, lang, other))
        } else {
            (Input::new("Incoming", &incoming, lang, other), Input::new("Current", &current, lang, head))
        };
        let (t1, t2) = if rebasing { (&current, &incoming) } else { (&incoming, &current) };
        let model = Merge::new(&base, t1, t2);
        let doc = self.docs[doc_id].as_mut().unwrap();
        let text = doc.buffer.text();
        let reset = text.lines().any(|l| l.starts_with("<<<<<<<"));
        let handled: Vec<[bool; 2]> = if reset {
            let merged = model.auto_merged();
            let end = doc.buffer.end();
            doc.buffer.edit(&[Selection::default()], &[(Pos::new(0, 0), end, &merged)], EditKind::Other);
            doc.buffer.break_undo_group();
            (0..model.ranges.len()).map(|i| [model.default_state(i).1; 2]).collect()
        } else {
            let regions = model.regions(&text);
            (0..model.ranges.len()).map(|i| [!model.is_conflicting(i) || !matches!(regions[i].state, State::Base | State::Manual); 2]).collect()
        };
        let mut ed = EditorState::new(doc_id);
        ed.merge = Some(Box::new(MergeState {
            path: path.to_path_buf(),
            model,
            inputs: [in1, in2],
            handled,
            regions: Vec::new(),
            regions_version: None,
            lenses: Lenses::default(),
            lens_actions: Vec::new(),
            input_actions: Vec::new(),
            input_scroll: [0.0; 2],
            opened: std::time::Instant::now(),
        }));
        let group = &mut self.groups[g];
        let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
        group.tabs.insert(at, ed);
        group.active = at;
        self.focus = super::Focus::Editor;
        self.merge_go_to_conflict(true);
    }

    /// The merge editor opened a moment ago (a click now finishes the one that opened it).
    pub(super) fn merge_just_opened(&self, g: usize) -> bool {
        let gr = &self.groups[g];
        gr.tabs.get(gr.active).and_then(|t| t.merge.as_ref()).is_some_and(|m| m.opened.elapsed() < std::time::Duration::from_millis(800))
    }

    /// Keeps a merge editor's view of the result up to date (before drawing it).
    pub(super) fn merge_refresh(&mut self, g: usize) {
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let (Some(m), Some(doc)) = (ed.merge.as_mut(), self.docs[ed.doc].as_ref()) else { return };
        m.refresh(doc);
    }

    /// Puts range `i` of group `g`'s merge editor in `state` (one undo step).
    fn merge_set(&mut self, g: usize, i: usize, state: State) {
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let Some(doc) = self.docs[ed.doc].as_mut() else { return };
        let Some(m) = ed.merge.as_mut() else { return };
        m.refresh(doc);
        let Some(region) = m.regions.get(i).cloned() else { return };
        let Some(new) = m.model.replacement(i, &region, state) else { return };
        let (from, to, text) = splice(&doc.buffer, region.lines.start, region.lines.end, &new);
        let before = ed.selections();
        doc.buffer.edit(&before, &[(from, to, &text)], EditKind::Other);
        doc.buffer.break_undo_group();
        let m = ed.merge.as_mut().unwrap();
        m.refresh(doc);
        m.handled[i] = [true, true];
        ed.clamp_selections(doc);
    }

    fn merge_do(&mut self, g: usize, action: MergeAction) {
        match action {
            MergeAction::Set(i, state) => self.merge_set(g, i, state),
            MergeAction::Ignore(i, input) => {
                let gr = &mut self.groups[g];
                if let Some(m) = gr.tabs.get_mut(gr.active).and_then(|t| t.merge.as_mut()) {
                    m.handled[i][input as usize - 1] = true;
                }
            }
        }
    }

    pub(super) fn click_merge_action(&mut self, g: usize, idx: usize) {
        self.active_group = g;
        let action = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.merge.as_ref()).and_then(|m| m.input_actions.get(idx)).map(|a| a.1);
        if let Some(a) = action {
            self.merge_do(g, a);
        }
    }

    /// A click on the result's summary row above a conflict.
    pub(super) fn run_merge_lens(&mut self, g: usize, index: usize) {
        let action = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.merge.as_ref()).and_then(|m| m.lens_actions.get(index)).copied();
        if let Some(a) = action {
            self.merge_do(g, a);
        }
    }

    /// Applies `pick` to every range at once (one undo step): Accept All Incoming / Current /
    /// Combination. `pick` gives a range's new state, or None to leave it.
    fn merge_set_all(&mut self, pick: impl Fn(&Merge, usize, State) -> Option<State>) {
        let Some((g, t)) = self.merge_tab() else { return };
        let ed = &mut self.groups[g].tabs[t];
        let Some(doc) = self.docs[ed.doc].as_mut() else { return };
        let Some(m) = ed.merge.as_mut() else { return };
        m.refresh(doc);
        let mut edits: Vec<(Pos, Pos, String)> = Vec::new();
        let mut touched = Vec::new();
        for (i, reg) in m.regions.iter().enumerate() {
            let Some(state) = pick(&m.model, i, reg.state).filter(|s| *s != reg.state) else { continue };
            if let Some(new) = m.model.replacement(i, reg, state) {
                edits.push(splice(&doc.buffer, reg.lines.start, reg.lines.end, &new));
                touched.push(i);
            }
        }
        if edits.is_empty() {
            return;
        }
        let before = ed.selections();
        let refs: Vec<(Pos, Pos, &str)> = edits.iter().map(|(a, b, s)| (*a, *b, s.as_str())).collect();
        doc.buffer.edit(&before, &refs, EditKind::Other);
        doc.buffer.break_undo_group();
        let m = ed.merge.as_mut().unwrap();
        m.refresh(doc);
        for i in touched {
            m.handled[i] = [true, true];
        }
        ed.clamp_selections(doc);
    }

    /// Merge Editor: Accept All Incoming Changes from Left / Current Changes from Right.
    pub(super) fn merge_accept_all(&mut self, input: u8) {
        self.merge_set_all(|m, i, _| m.ranges[i].changed(input).then_some(if input == 1 { State::Input1 } else { State::Input2 }));
    }

    /// Merge Editor: Accept All Combination.
    pub(super) fn merge_accept_all_combination(&mut self) {
        self.merge_set_all(|m, i, _| (m.is_conflicting(i) && m.can_be_combined(i)).then_some(State::Both { first: 1, smart: true }));
    }

    /// Merge Editor: Reset Result: the automatic merge again, conflicts unhandled.
    pub(super) fn merge_reset(&mut self) {
        let Some((g, t)) = self.merge_tab() else { return };
        let ed = &mut self.groups[g].tabs[t];
        let Some(doc) = self.docs[ed.doc].as_mut() else { return };
        let Some(m) = ed.merge.as_mut() else { return };
        let merged = m.model.auto_merged();
        let end = doc.buffer.end();
        let before = ed.selections();
        doc.buffer.edit(&before, &[(Pos::new(0, 0), end, &merged)], EditKind::Other);
        doc.buffer.break_undo_group();
        let m = ed.merge.as_mut().unwrap();
        m.refresh(doc);
        m.handled = (0..m.model.ranges.len()).map(|i| [m.model.default_state(i).1; 2]).collect();
        ed.clamp_selections(doc);
    }

    /// Go to Next / Previous Unhandled Conflict (wrapping around).
    pub(super) fn merge_go_to_conflict(&mut self, forward: bool) {
        let Some((g, t)) = self.merge_tab() else { return };
        self.merge_refresh(g);
        let ed = &mut self.groups[g].tabs[t];
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let Some(m) = ed.merge.as_ref() else { return };
        let starts: Vec<usize> = (0..m.regions.len()).filter(|&i| m.conflicting(i) && !m.handled(i)).map(|i| m.regions[i].lines.start).collect();
        let line = ed.sel.head.line;
        let pick = if forward {
            starts.iter().find(|&&s| s > line).or(starts.first())
        } else {
            starts.iter().rev().find(|&&s| s < line).or(starts.last())
        };
        match pick {
            Some(&s) => ed.jump_to(doc, Pos::new(s.min(doc.buffer.len_lines().saturating_sub(1)), 0)),
            None => self.set_status_message("All conflicts handled, the merge can be completed now."),
        }
    }

    /// Complete Merge: saves the result (with conflict markers around conflicts still not
    /// handled, if the user goes ahead anyway), stages it and closes the merge editor.
    pub(super) fn complete_merge(&mut self) {
        let Some((g, t)) = self.merge_tab() else { return };
        self.merge_refresh(g);
        let (doc_id, remaining, path) = {
            let ed = &self.groups[g].tabs[t];
            let m = ed.merge.as_ref().unwrap();
            (ed.doc, m.remaining(), m.path.clone())
        };
        if remaining > 0 {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let answer = self
                .message_dialog()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(format!("Do you want to complete the merge of {name}?"))
                .set_description("The file contains unhandled conflicts.")
                .set_buttons(rfd::MessageButtons::OkCancelCustom("Complete with Conflicts".into(), "Cancel".into()))
                .show();
            if !matches!(answer, rfd::MessageDialogResult::Custom(ref s) if s == "Complete with Conflicts") && answer != rfd::MessageDialogResult::Ok {
                return;
            }
            // Put markers back around what's still unresolved, so nothing is lost.
            let ed = &mut self.groups[g].tabs[t];
            let m = ed.merge.as_ref().unwrap();
            let unhandled: Vec<bool> = (0..m.model.ranges.len()).map(|i| m.conflicting(i) && !m.handled(i)).collect();
            if let Some(doc) = self.docs[doc_id].as_mut() {
                let marked = m.model.with_markers(&doc.buffer.text(), &unhandled);
                let end = doc.buffer.end();
                let before = ed.selections();
                doc.buffer.edit(&before, &[(Pos::new(0, 0), end, &marked)], EditKind::Other);
                doc.buffer.break_undo_group();
                ed.clamp_selections(doc);
            }
        }
        if self.docs[doc_id].as_ref().is_some_and(|d| d.buffer.is_dirty()) && !self.save_doc(doc_id) {
            return;
        }
        // Close the merge editor without closing the document if another tab shows it.
        let still_open = self.groups.iter().enumerate().any(|(gi, gr)| gr.tabs.iter().enumerate().any(|(ti, tab)| tab.doc == doc_id && (gi, ti) != (g, t)));
        if still_open {
            let gr = &mut self.groups[g];
            gr.tabs.remove(t);
            gr.active = gr.active.min(gr.tabs.len().saturating_sub(1));
        } else {
            self.close_tab(g, t);
        }
        self.git_run(scm::Op::Stage(vec![path]));
        self.show_view(super::View::Scm);
    }

    /// Whether the active editor's file is in conflict in git (for Resolve in Merge Editor).
    pub(super) fn is_conflicted(&self, path: &Path) -> bool {
        self.repo.as_ref().is_some_and(|r| r.status.conflicts.iter().any(|c| c.path == path))
    }

    /// The result's decorations: changed ranges and conflict borders (the one holding the
    /// caret more strongly).
    pub(super) fn merge_marks(&self, g: usize) -> Vec<MergeMark> {
        let gr = &self.groups[g];
        let Some(ed) = gr.tabs.get(gr.active) else { return Vec::new() };
        let Some(m) = ed.merge.as_ref() else { return Vec::new() };
        let caret = ed.sel.head.line;
        let change = self.color("mergeEditor.change.background");
        m.regions
            .iter()
            .enumerate()
            .filter(|(i, r)| m.conflicting(*i) || r.state != State::Base)
            .map(|(i, r)| {
                let focused = r.lines.contains(&caret) || (r.lines.is_empty() && r.lines.start == caret);
                let border = match (m.conflicting(i), m.handled(i), focused) {
                    (false, ..) => Color::TRANSPARENT,
                    (true, true, false) => self.color("mergeEditor.conflict.handledUnfocused.border"),
                    (true, true, true) => self.color("mergeEditor.conflict.handledFocused.border"),
                    (true, false, false) => self.color("mergeEditor.conflict.unhandledUnfocused.border"),
                    (true, false, true) => self.color("mergeEditor.conflict.unhandledFocused.border"),
                };
                let background = if r.state == State::Base { Color::TRANSPARENT } else { change };
                MergeMark { lines: r.lines.clone(), background, border }
            })
            .collect()
    }

    /// A pane's title bar: "Incoming  ◉ abc1234  main".
    fn merge_header(&mut self, c: &mut Canvas, r: Rect, title: &str, description: &str, detail: &str, commit: bool) {
        c.fill(r, self.color("editor.background"));
        c.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), self.color_or("editorGroupHeader.tabsBorder", "editorGroup.border"));
        let fg = self.color("foreground");
        let dim = self.color("descriptionForeground");
        let mut x = r.x + 10.0;
        let y = r.y + (r.h - 18.0) / 2.0;
        let st = TextStyle::ui(UI, fg).weight(600);
        c.text(x, y, title, &st);
        x += c.measure(title, &st) + 10.0;
        if !description.is_empty() {
            if commit {
                c.icon_in(&icons::GIT_COMMIT, Rect::new(x, r.y, 16.0, r.h), 14.0, dim);
                x += 18.0;
            }
            let st = TextStyle::ui(UI, dim);
            c.text(x, y, description, &st);
            x += c.measure(description, &st) + 10.0;
        }
        if !detail.is_empty() {
            c.text_in(Rect::new(x, y, (r.right() - x - 8.0).max(0.0), 18.0), detail, &TextStyle::ui(UI, dim).italic(true));
        }
    }

    /// The two inputs, side by side.
    pub(super) fn draw_merge_inputs(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        let half = (r.w / 2.0).floor();
        // Line the inputs up with the result's first visible line.
        let (top_line, frac) = {
            let gr = &self.groups[g];
            let Some(ed) = gr.tabs.get(gr.active) else { return };
            let Some(doc) = self.docs[ed.doc].as_ref() else { return };
            let lh = line_height();
            let row = (ed.scroll_y / lh).floor() as usize;
            let rows = ed.layout.rows_of_line(doc.buffer.len_lines().saturating_sub(1)).end;
            let vr = ed.layout.row(row.min(rows.saturating_sub(1)), &doc.buffer);
            (vr.line, if vr.zone { 0.0 } else { ed.scroll_y - row as f32 * lh })
        };
        let gr = &mut self.groups[g];
        if let Some(m) = gr.tabs.get_mut(gr.active).and_then(|t| t.merge.as_mut()) {
            m.input_actions.clear();
        }
        for k in 0..2 {
            let pane = if k == 0 { Rect::new(r.x, r.y, half, r.h) } else { Rect::new(r.x + half + 1.0, r.y, r.w - half - 1.0, r.h) };
            self.draw_merge_input(c, g, k, pane, top_line, frac);
        }
        let rects: Vec<Rect> = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.merge.as_ref()).map(|m| m.input_actions.iter().map(|a| a.0).collect()).unwrap_or_default();
        self.hits.extend(rects.into_iter().enumerate().map(|(i, r)| (r, Hit::MergeAction(g, i))));
        c.fill(Rect::new(r.x + half, r.y, 1.0, r.h), self.color_or("editorGroup.border", "widget.border"));
    }

    fn draw_merge_input(&mut self, c: &mut Canvas, g: usize, k: usize, r: Rect, top_line: usize, frac: f32) {
        let (header, body) = r.cut_top(HEADER_H);
        let (title, description, detail) = match self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.merge.as_ref()) {
            Some(m) => (m.inputs[k].title, m.inputs[k].description.clone(), m.inputs[k].detail.clone()),
            None => return,
        };
        self.merge_header(c, header, title, &description, &detail, true);
        c.fill(body, self.color("editor.background"));
        self.hits.push((body, Hit::MergeInput(g, k as u8)));

        let lh = line_height();
        let fg = self.color("editor.foreground");
        let style = TextStyle::mono(font_size(), lh, fg);
        let numbers = style.color(self.color("editorLineNumber.foreground"));
        let lens_style = TextStyle::ui(font_size() * 0.9, self.color("editorCodeLens.foreground"));
        let lens_hover = self.color("editorLink.activeForeground");
        let change_bg = self.color("mergeEditor.change.background");
        let colors = [
            self.color("mergeEditor.conflict.unhandledUnfocused.border"),
            self.color("mergeEditor.conflict.unhandledFocused.border"),
            self.color("mergeEditor.conflict.handledUnfocused.border"),
            self.color("mergeEditor.conflict.handledFocused.border"),
        ];
        let (box_bg, box_border, box_fg) = (self.color("checkbox.background"), self.color("checkbox.border"), self.color("checkbox.foreground"));
        let mouse = self.mouse;
        let hovered = |r: &Rect| r.contains(mouse.0, mouse.1);
        let push_action = |actions: &mut Vec<(Rect, MergeAction)>, r: Rect, a: MergeAction| {
            let r = r.intersect(&body);
            if r.w > 0.0 && r.h > 0.0 {
                actions.push((r, a));
            }
        };
        let theme = &self.theme;
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let scroll_x = ed.scroll_x;
        let caret = ed.sel.head.line;
        let Some(m) = ed.merge.as_mut() else { return };
        let input = k as u8 + 1;
        let rows = m.input_rows(k);
        // Scroll so the line matching the result's top line is at the top.
        let line = m.input_line_for(k, top_line);
        let row_of = |l: usize| rows.iter().position(|r| *r == Row::Line(l)).unwrap_or(0);
        let top_row = rows.iter().position(|r| *r == Row::Line(line)).map_or(0, |p| {
            // Show the actions row above a conflict when the result shows its summary row.
            if p > 0 && matches!(rows[p - 1], Row::Actions(_)) && frac == 0.0 && m.regions.iter().any(|reg| reg.lines.start == top_line) { p - 1 } else { p }
        });
        let scroll = (top_row as f32 * lh + frac).min((rows.len().saturating_sub(1)) as f32 * lh);
        m.input_scroll[k] = scroll;

        let cw = c.measure("0000000000", &style) / 10.0;
        let digits = m.input_lines(k).to_string().len().max(3) as f32;
        let gutter_w = 8.0 + CHECKBOX + 8.0 + digits * cw + 14.0;
        let text_x = body.x + gutter_w - scroll_x;
        let first = (scroll / lh).floor() as usize;
        let last = (((scroll + body.h) / lh).ceil() as usize + 1).min(rows.len());
        let y_of = |row: usize| body.y + row as f32 * lh - scroll;

        // Highlight spans for the visible lines.
        let visible: Vec<usize> = rows[first.min(rows.len())..last].iter().filter_map(|r| if let Row::Line(l) = r { Some(*l) } else { None }).collect();
        let (l0, l1) = (visible.first().copied().unwrap_or(0), visible.last().map_or(0, |l| l + 1));
        let inp = &mut m.inputs[k];
        let spans = inp.highlight.spans(&inp.buffer, l0, l1);

        c.push_clip(body);
        // Backgrounds of the lines this input changed.
        for range in m.model.ranges.iter() {
            for lines in range.changed_lines(input).filter(|l| !l.is_empty()) {
                if lines.end <= l0 || lines.start >= l1.max(l0 + 1) {
                    continue;
                }
                let (y0, y1) = (y_of(row_of(lines.start)), y_of(row_of(lines.end - 1)) + lh);
                c.fill(Rect::new(body.x + gutter_w - 4.0, y0, body.w - gutter_w + 4.0, y1 - y0), change_bg);
            }
        }
        // Lines: numbers and text.
        for (i, row) in rows[first.min(rows.len())..last].iter().enumerate() {
            let y = y_of(first + i);
            let Row::Line(l) = *row else { continue };
            let number = (l + 1).to_string();
            let nw = c.measure(&number, &numbers);
            c.text(body.x + gutter_w - 14.0 - nw, y, &number, &numbers);
            let text = m.inputs[k].buffer.line(l);
            let sp: Vec<Span> = spans.get(l - l0).cloned().unwrap_or_default();
            let (display, sp) = expand_tabs_spans(&text, &sp);
            let colored: Vec<(usize, usize, Color)> = sp.iter().map(|(a, b, t)| (*a, *b, theme.token(*t))).collect();
            c.push_clip(Rect::new(body.x + gutter_w - 4.0, body.y, body.w - gutter_w + 4.0, body.h));
            c.rich_text(text_x, y, &display, &colored, &style);
            c.pop_clip();
        }
        // Conflict borders, checkboxes and actions.
        let n = m.input_lines(k);
        for i in 0..m.model.ranges.len() {
            let range = m.model.ranges[i].input(input);
            let conflicting = m.conflicting(i);
            let handled = m.handled(i);
            let start_row = row_of(range.start.min(n.saturating_sub(1)));
            if start_row + 1 < first || start_row > last + 1 {
                continue;
            }
            let focused = m.regions.get(i).is_some_and(|reg| reg.lines.contains(&caret) || (reg.lines.is_empty() && reg.lines.start == caret));
            if conflicting {
                let border = colors[usize::from(handled) * 2 + usize::from(focused)];
                let x = body.x + gutter_w - 6.0;
                if range.is_empty() {
                    c.fill(Rect::new(x, y_of(start_row) - 1.0, body.right() - x, 2.0), border);
                } else {
                    let y1 = y_of(row_of(range.end - 1)) + lh;
                    c.bordered(Rect::new(x, y_of(start_row), body.right() - x - 1.0, y1 - y_of(start_row)), Color::TRANSPARENT, border, 1.0, 0.0);
                }
                // The actions row above.
                let mut x = text_x.max(body.x + gutter_w);
                let y = y_of(start_row) - lh;
                let sep_w = c.measure(" | ", &lens_style);
                for (j, (label, action)) in m.input_actions_for(i, k).into_iter().enumerate() {
                    if j > 0 {
                        c.text_in(Rect::new(x, y, sep_w, lh), " | ", &lens_style);
                        x += sep_w;
                    }
                    let w = c.measure(&label, &lens_style);
                    let ar = Rect::new(x, y, w, lh);
                    c.text_in(ar, &label, &if hovered(&ar) { lens_style.color(lens_hover) } else { lens_style });
                    push_action(&mut m.input_actions, ar, action);
                    x += w;
                }
            }
            // The checkbox takes (or drops) this side's change.
            if m.model.ranges[i].changed(input) {
                let state = m.regions.get(i).map_or(State::Base, |r| r.state);
                let bx = Rect::new(body.x + 8.0, y_of(start_row) + (lh - CHECKBOX) / 2.0, CHECKBOX, CHECKBOX);
                c.bordered(bx, box_bg, box_border, 1.0, 3.0);
                let (icon, next) = match state {
                    State::Manual => (Some(&icons::DOT), State::Base.with(input, true)),
                    State::Both { first, .. } if first != input => (Some(&icons::CHECK_ALL), state.with(input, false)),
                    s if s.includes(input) => (Some(&icons::CHECK), s.with(input, false)),
                    s => (None, s.with(input, true)),
                };
                if let Some(icon) = icon {
                    c.icon_in(icon, bx, 12.0, box_fg);
                }
                push_action(&mut m.input_actions, Rect::new(bx.x - 2.0, bx.y - 2.0, bx.w + 4.0, bx.h + 4.0), MergeAction::Set(i, next));
            }
        }
        c.pop_clip();
    }

    /// The result's title bar: "Result  src/main.rs   2 Conflicts Remaining".
    pub(super) fn draw_merge_result_header(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        let Some(m) = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.merge.as_ref()) else { return };
        let remaining = m.remaining();
        let path = m.path.clone();
        let rel = self.repo.as_ref().and_then(|repo| path.strip_prefix(&repo.root).ok()).unwrap_or(&path).to_string_lossy().into_owned();
        self.merge_header(c, r, "Result", &rel, "", false);
        let text = if remaining == 1 { "1 Conflict Remaining".to_string() } else { format!("{remaining} Conflicts Remaining") };
        let hit = Hit::MergeRemaining(g);
        let color = if self.hovered(hit) { self.color("textLink.activeForeground") } else { self.color("descriptionForeground") };
        let st = TextStyle::ui(UI, color);
        let w = c.measure(&text, &st);
        let tr = Rect::new(r.right() - w - 16.0, r.y, w, r.h);
        c.text_in(Rect::new(tr.x, r.y + (r.h - 18.0) / 2.0, w, 18.0), &text, &st);
        self.hits.push((tr, hit));
    }

    /// The Complete Merge button, over the result's bottom right corner.
    pub(super) fn draw_merge_complete(&mut self, c: &mut Canvas, g: usize, result: Rect) {
        let label = "Complete Merge";
        let st = TextStyle::ui(UI, self.color("button.foreground"));
        let w = c.measure(label, &st) + 26.0;
        let r = Rect::new(result.right() - 14.0 - 16.0 - w, result.bottom() - 16.0 - 26.0, w, 26.0);
        let hit = Hit::MergeComplete(g);
        let bg = self.color(if self.hovered(hit) { "button.hoverBackground" } else { "button.background" });
        c.bordered(r, bg, self.color_or("button.border", "contrastBorder"), 1.0, super::controls::FIELD_RADIUS);
        c.text_in(Rect::new(r.x + 13.0, r.y + 4.0, w - 26.0, 18.0), label, &st);
        self.hits.push((r, hit));
    }

    /// "Resolve in Merge Editor", over the bottom right corner of an editor with conflicts.
    pub(super) fn draw_resolve_in_merge_editor(&mut self, c: &mut Canvas, g: usize, editor: Rect) {
        let label = "Resolve in Merge Editor";
        let st = TextStyle::ui(UI, self.color("button.foreground"));
        let w = c.measure(label, &st) + 26.0;
        let r = Rect::new(editor.right() - 14.0 - 16.0 - w, editor.bottom() - 16.0 - 26.0, w, 26.0);
        let hit = Hit::OpenMergeEditor(g);
        let bg = self.color(if self.hovered(hit) { "button.hoverBackground" } else { "button.background" });
        c.bordered(r, bg, self.color_or("button.border", "contrastBorder"), 1.0, super::controls::FIELD_RADIUS);
        c.text_in(Rect::new(r.x + 13.0, r.y + 4.0, w - 26.0, 18.0), label, &st);
        self.hits.push((r, hit));
    }
}
