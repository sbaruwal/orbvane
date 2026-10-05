//! The code action lightbulb: when the cursor rests, ask the language server for code actions
//! there and, if it has some, show a lightbulb in the gutter.
//! Clicking it opens the same menu as Quick Fix (⌘.).

use std::time::{Duration, Instant};

use lsp::Encoding;
use text::Selection;

use super::Workbench;
use crate::config::{self, Lightbulb as Mode};
use crate::servers::ServerKey;

/// How long the cursor rests before asking.
const DELAY: Duration = Duration::from_millis(250);

/// Where the cursor is: (group, document, buffer version, primary selection).
type Spot = (usize, usize, u64, Selection);

#[derive(Default)]
pub(super) struct Lightbulb {
    /// The spot being asked about, and the request number for it.
    asked: Option<Spot>,
    seq: u64,
    /// When to send the request for `asked`.
    due: Option<Instant>,
    shown: Option<Shown>,
    /// The servers' `work_done` count when last checked.
    work_done: u64,
}

/// The actions behind a visible lightbulb.
struct Shown {
    g: usize,
    doc: usize,
    version: u64,
    line: usize,
    actions: Vec<lsp::CodeAction>,
    encoding: Encoding,
    key: ServerKey,
    /// A preferred quick fix exists (lightbulb gets the autofix look).
    autofix: bool,
}

impl Lightbulb {
    /// Drops the request and the actions shown (their documents are being closed).
    pub(super) fn forget_docs(&mut self) {
        self.asked = None;
        self.due = None;
        self.shown = None;
        self.seq += 1;
    }
}

impl Workbench {
    /// Where the lightbulb could show now: the active editor's cursor, in a document whose
    /// server offers code actions.
    fn lightbulb_spot(&self) -> Option<Spot> {
        let mode = config::get().lightbulb;
        if mode == Mode::Off {
            return None;
        }
        let ed = self.active_editor().filter(|e| !e.is_special())?;
        let doc = self.docs[ed.doc].as_ref()?;
        let path = doc.buffer.path()?;
        if !self.lsp.has_server(path) || !self.lsp.supports(path, "codeActionProvider") {
            return None;
        }
        if mode == Mode::OnCode && ed.sel.is_empty() && doc.buffer.line(ed.sel.head.line).trim().is_empty() {
            return None;
        }
        Some((self.active_group, ed.doc, doc.buffer.version(), ed.sel))
    }

    /// Follows the cursor: hides the lightbulb when it leaves the line, and asks for actions
    /// once it has rested. Called every frame.
    pub(super) fn lightbulb_tick(&mut self) {
        let spot = self.lightbulb_spot();
        let lb = &mut self.lightbulb;
        if let Some(sh) = &lb.shown {
            if spot.is_none_or(|(g, doc, _, sel)| (g, doc, sel.head.line) != (sh.g, sh.doc, sh.line)) {
                lb.shown = None;
            }
        }
        if spot != lb.asked {
            lb.asked = spot;
            lb.seq += 1;
            lb.due = spot.map(|_| Instant::now() + DELAY);
        }
        if lb.due.is_some_and(|t| Instant::now() >= t) {
            lb.due = None;
            let seq = lb.seq;
            if let Some((_, doc, _, sel)) = spot {
                self.request_code_actions(doc, sel.ordered(), Some(seq));
            }
        }
    }

    /// Asks again when the answer may have changed without the cursor moving: new
    /// diagnostics for the document, or the server finished
    /// some work (while loading, servers answer with nothing).
    pub(super) fn lightbulb_refresh(&mut self) {
        let work_done = std::mem::replace(&mut self.lightbulb.work_done, self.lsp.work_done);
        let Some((_, doc, _, _)) = self.lightbulb.asked else { return };
        let Some(path) = self.docs.get(doc).and_then(Option::as_ref).and_then(|d| d.buffer.path()) else { return };
        if work_done != self.lsp.work_done || self.lsp.published.iter().any(|p| p == path) {
            self.lightbulb.asked = None;
        }
    }

    pub(super) fn lightbulb_deadline(&self) -> Option<Instant> {
        self.lightbulb.due
    }

    /// The server's answer for request `seq`: shows the lightbulb if there's something to do
    /// (source actions like Organize Imports don't count).
    pub(super) fn lightbulb_actions(&mut self, seq: u64, actions: Vec<lsp::CodeAction>, encoding: Encoding, key: ServerKey) {
        let lb = &mut self.lightbulb;
        let Some((g, doc, version, sel)) = lb.asked.filter(|_| seq == lb.seq) else { return };
        let actions: Vec<lsp::CodeAction> =
            actions.into_iter().filter(|a| a.disabled.is_none() && !a.kind.starts_with("source")).collect();
        if actions.is_empty() {
            lb.shown = None;
            return;
        }
        let autofix = actions.iter().any(|a| a.preferred && a.kind.starts_with("quickfix"));
        lb.shown = Some(Shown { g, doc, version, line: sel.head.line, actions, encoding, key, autofix });
    }

    /// The lightbulb to draw in group `g`'s editor showing `doc`: (line, autofix).
    pub(super) fn lightbulb_for(&self, g: usize, doc: usize) -> Option<(usize, bool)> {
        self.lightbulb.shown.as_ref().filter(|sh| sh.g == g && sh.doc == doc).map(|sh| (sh.line, sh.autofix))
    }

    /// A click on the lightbulb: the actions menu below it. If the text changed since the
    /// actions came, ask again (as Quick Fix) so a stale edit is never applied.
    pub(super) fn open_lightbulb(&mut self, g: usize) {
        self.active_group = g;
        let Some(sh) = &self.lightbulb.shown else { return };
        let fresh = self.lightbulb_spot().is_some_and(|(sg, doc, version, _)| (sg, doc, version) == (sh.g, sh.doc, sh.version));
        let anchor = self.groups[g].tabs.get(self.groups[g].active).and_then(|e| e.geom.lightbulb);
        match anchor.filter(|_| fresh) {
            Some(r) => {
                let (actions, encoding, key) = (sh.actions.clone(), sh.encoding, sh.key.clone());
                self.code_actions_menu(actions, encoding, key, r.x, r.bottom());
            }
            None => self.quick_fix(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_folders_forgets_the_cursor_it_asked_about() {
        let dir = std::env::temp_dir().join(format!("orbvane-lightbulb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for f in ["one.txt", "two.txt", "three.txt"] {
            std::fs::write(a.join(f), "text\n").unwrap();
        }
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let files: Vec<_> = ["one.txt", "two.txt", "three.txt"].iter().map(|f| a.join(f)).collect();
        let mut wb = Workbench::new(Some(a.clone()), &files, std::sync::Arc::new(|| {}));
        // The lightbulb asked about the third document's cursor (what crashed: the index
        // outlived the documents).
        let sel = wb.active_editor().unwrap().sel;
        wb.lightbulb.asked = Some((0, 2, 1, sel));
        assert!(wb.switch_folder(&b));
        wb.lsp_tick();
        assert!(wb.lightbulb.asked.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
