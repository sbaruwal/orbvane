//! Git blame in the editor: after the line the caret is on, who last changed it, how long ago
//! and the commit's summary (`git.blame.editorDecoration.enabled`). Clicking it opens what that
//! commit changed in the file. The active document is blamed with its own text (unsaved changes
//! show as not committed yet) once typing pauses, and again when HEAD moves (a commit, a
//! checkout), meanwhile showing the blame it had.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Workbench;
use crate::diff_view::DiffSpec;

/// How long typing must pause before the document is blamed again.
const PAUSE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct BlameState {
    /// By document: its path, the version blamed and the answer (None: git can't blame it).
    docs: HashMap<usize, (PathBuf, u64, Option<Arc<scm::Blame>>)>,
    /// The request out: (document, version).
    asked: Option<(usize, u64)>,
    /// The active document's version, and since when (requests wait for a pause).
    seen: Option<(usize, u64, Instant)>,
    /// Documents whose blame was made at an older HEAD (shown until the new one comes).
    stale: HashSet<usize>,
    /// The HEAD commit the blames were made at.
    head: Option<String>,
}

impl BlameState {
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
        self.asked = None;
        self.seen = None;
        self.stale.clear();
    }
}

/// How the annotation reads: "Ann, 3 days ago • Fix the parser".
pub(super) fn describe(c: &scm::BlameCommit, now: i64) -> String {
    if c.uncommitted() {
        return "You, Uncommitted changes".into();
    }
    let age = super::timeline::age(now - c.time);
    let when = if age == "now" { "just now".to_string() } else { format!("{age} ago") };
    format!("{}, {when} • {}", c.author, c.summary)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Workbench {
    /// The active text editor's document: (id, path, version), when its file is in the repo.
    fn blame_target(&self) -> Option<(usize, PathBuf, u64)> {
        let g = &self.groups[self.active_group];
        let ed = g.tabs.get(g.active).filter(|e| !e.is_special() && e.search.is_none() && e.merge.is_none())?;
        let doc = self.docs.get(ed.doc)?.as_ref()?;
        let path = doc.buffer.path()?.to_path_buf();
        let repo = self.repo.as_ref()?;
        path.starts_with(&repo.root).then(|| (ed.doc, path, doc.buffer.version()))
    }

    /// Asks git to blame the active document when its blame is missing or out of date.
    pub(super) fn blame_tick(&mut self) {
        if !crate::config::get().blame {
            return;
        }
        let Some((id, path, version)) = self.blame_target() else { return };
        let b = &mut self.blame;
        let current = b.docs.get(&id).is_some_and(|(p, v, _)| *p == path && *v == version) && !b.stale.contains(&id);
        if current || b.asked == Some((id, version)) {
            return;
        }
        if b.seen.is_none_or(|(i, v, _)| (i, v) != (id, version)) {
            b.seen = Some((id, version, Instant::now()));
        }
        // A document blamed before waits for typing to pause; a new one is asked at once.
        let known = b.docs.get(&id).is_some_and(|(p, _, _)| *p == path);
        if known && b.seen.is_some_and(|(_, _, t)| t.elapsed() < PAUSE) {
            return;
        }
        if b.asked.is_some() {
            return; // one at a time; this one is asked when that answer comes
        }
        let doc = self.docs[id].as_ref().unwrap();
        let contents = doc.buffer.is_dirty().then(|| doc.buffer.text());
        b.asked = Some((id, version));
        if let Some(repo) = &self.repo {
            repo.send(scm::Job::Blame { path, contents, tag: version });
        }
    }

    pub(super) fn blame_arrived(&mut self, path: PathBuf, version: u64, blame: Option<scm::Blame>) {
        let b = &mut self.blame;
        let Some((id, _)) = b.asked.filter(|&(id, _)| self.docs.get(id).and_then(|d| d.as_ref()).is_some_and(|d| d.buffer.path() == Some(path.as_path()))) else {
            b.asked = None;
            return;
        };
        b.asked = None;
        b.stale.remove(&id);
        b.docs.insert(id, (path, version, blame.map(Arc::new)));
    }

    /// Git's status arrived: when HEAD moved (a commit, a checkout), every blame is asked again.
    pub(super) fn refresh_blame(&mut self) {
        let head = self.repo.as_ref().and_then(|r| r.status.head.clone());
        if head != self.blame.head {
            self.blame.head = head;
            self.blame.stale.extend(self.blame.docs.keys().copied());
        }
    }

    pub(super) fn blame_deadline(&self) -> Option<Instant> {
        if !crate::config::get().blame {
            return None;
        }
        let (_, _, t) = self.blame.seen?;
        let (id, _, version) = self.blame_target()?;
        let current = self.blame.docs.get(&id).is_some_and(|e| e.1 == version) && !self.blame.stale.contains(&id);
        (!current && self.blame.asked.is_none()).then_some(t + PAUSE)
    }

    /// The commit that last changed `line` of document `doc`, when its blame is up to date.
    fn blame_commit(&self, doc: usize, line: usize) -> Option<&scm::BlameCommit> {
        let (path, version, blame) = self.blame.docs.get(&doc)?;
        let d = self.docs.get(doc)?.as_ref()?;
        if d.buffer.version() != *version || d.buffer.path() != Some(path.as_path()) {
            return None;
        }
        let blame = blame.as_ref()?;
        blame.commits.get(*blame.lines.get(line)?)
    }

    /// The annotation for group `g`'s editor: (line, text), on its caret's line.
    pub(super) fn blame_annotation(&self, g: usize) -> Option<(usize, String)> {
        if !crate::config::get().blame || g != self.active_group {
            return None;
        }
        let gr = &self.groups[g];
        let ed = gr.tabs.get(gr.active)?;
        if !ed.extra.is_empty() || ed.sel.anchor != ed.sel.head {
            return None;
        }
        let line = ed.sel.head.line;
        Some((line, describe(self.blame_commit(ed.doc, line)?, unix_now())))
    }

    /// The annotation of group `g` was clicked: what its commit changed in the file.
    pub(super) fn open_blame_commit(&mut self, g: usize) {
        let gr = &self.groups[g];
        let Some(ed) = gr.tabs.get(gr.active) else { return };
        let Some(c) = self.blame_commit(ed.doc, ed.sel.head.line).filter(|c| !c.uncommitted()) else { return };
        let revision = Some(c.hash.clone());
        let Some(path) = self.docs[ed.doc].as_ref().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        self.open_diff(DiffSpec { path, staged: false, revision, left_file: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_commits() {
        let c = scm::BlameCommit { hash: "ab12".into(), author: "Ann".into(), email: String::new(), time: 1_000_000, summary: "Fix the parser".into() };
        assert_eq!(describe(&c, 1_000_000 + 3 * 86_400), "Ann, 3 days ago • Fix the parser");
        assert_eq!(describe(&c, 1_000_010), "Ann, just now • Fix the parser");
        let mine = scm::BlameCommit { hash: "0".repeat(40), ..c };
        assert_eq!(describe(&mine, 0), "You, Uncommitted changes");
    }
}
