//! Source Control: the side bar view (commit box, staged/unstaged changes), git colors in
//! the explorer and tabs, and change markers in the editor gutter.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use render::{Canvas, Color, Icon, Rect, TextStyle};
use scm::{Change, Event, FileStatus, Job, LineChange, Op, Repo};

use super::controls::{FIELD_H, FIELD_RADIUS};
use super::{Focus, Hit, View, Workbench, ROW_H, SMALL, UI};
use crate::diff_view::{DiffSpec, DiffState};
use crate::editor::{Doc, EditorState};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::{FieldEvent, TextField};

/// How often to re-check `git status` while the window is focused (polling
/// instead of file watchers keeps us dependency-free).
const POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Section {
    Merge,
    Staged,
    Changes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScmAction {
    Commit,
    /// The chevron next to the commit button (Commit & Push, Amend...).
    CommitMenu,
    /// The view's "..." menu.
    More,
    Publish,
    Sync,
    ContinueRebase,
    Refresh,
    InitRepo,
    /// No git: `xcode-select --install` in a terminal.
    InstallGit,
    StageAll,
    UnstageAll,
    DiscardAll,
    ToggleSection(Section),
    Open(Section, usize),
    Stage(Section, usize),
    Unstage(usize),
    Discard(Section, usize),
}

#[derive(Default)]
pub(super) struct ScmView {
    pub(super) message: TextField,
    /// Where the "..." and commit chevron buttons were drawn (popup menus open below them).
    more_rect: Rect,
    commit_menu_rect: Rect,
    collapsed: HashSet<Section>,
    scroll: f32,
    /// HEAD contents per file (None: not tracked at HEAD), for gutter diffs.
    head_cache: HashMap<PathBuf, Option<Arc<String>>>,
    head_requested: HashSet<PathBuf>,
    /// HEAD commit the cache belongs to.
    cached_head: Option<String>,
    pub(super) last_refresh: Option<Instant>,
    /// Gutter markers per document: (doc buffer version, markers).
    marks: HashMap<PathBuf, (u64, Vec<LineChange>)>,
    /// File rows from the last draw: (row, section, index in section).
    rows: Vec<(usize, Section, usize)>,
}

/// A repository the Source Control view isn't showing, with its view state.
pub(super) struct ParkedRepo {
    pub(super) repo: Repo,
    scm: ScmView,
    graph: super::scm_graph::ScmGraph,
    git: super::git_actions::GitState,
    /// Events that arrived while parked (other than status).
    pending: Vec<Event>,
}

pub(super) fn status_color(theme: &theme::Theme, s: FileStatus) -> Color {
    theme.color(match s {
        FileStatus::Modified | FileStatus::TypeChanged => "gitDecoration.modifiedResourceForeground",
        FileStatus::Added => "gitDecoration.addedResourceForeground",
        FileStatus::Deleted => "gitDecoration.deletedResourceForeground",
        FileStatus::Renamed | FileStatus::Copied => "gitDecoration.renamedResourceForeground",
        FileStatus::Untracked => "gitDecoration.untrackedResourceForeground",
        FileStatus::Conflicted => "gitDecoration.conflictingResourceForeground",
    })
}

impl Workbench {
    /// Opens the git repositories of the workspace's folders (one per repository, in folder
    /// order). The first is the one the Source Control view shows; the others wait, parked.
    pub(super) fn open_repos(&mut self, folders: &[PathBuf]) {
        self.repo = None;
        self.parked_repos.clear();
        self.repo_roots.clear();
        self.repo_followed = None;
        self.scm = ScmView::default();
        let (open, height) = (self.scm_graph.open, self.scm_graph.height);
        self.scm_graph = super::scm_graph::ScmGraph::default();
        (self.scm_graph.open, self.scm_graph.height) = (open, height);
        self.scm.last_refresh = Some(Instant::now());
        self.reset_git_state();
        for folder in folders {
            let Some(repo) = Repo::open(folder, self.waker.clone()) else { continue };
            if self.repo_roots.contains(&repo.root) {
                continue;
            }
            self.repo_roots.push(repo.root.clone());
            if self.repo.is_none() {
                self.repo = Some(repo);
            } else {
                self.parked_repos.push(ParkedRepo { repo, scm: ScmView { last_refresh: Some(Instant::now()), ..Default::default() }, graph: Default::default(), git: Default::default(), pending: Vec::new() });
            }
        }
    }

    /// Initialize Repository in a folder: the workspace's repositories are opened again.
    pub(super) fn open_repo(&mut self, _folder: &Path) {
        let folders = self.folders();
        self.open_repos(&folders);
    }

    /// Shows repository `root` in the Source Control view; the one shown is parked with its
    /// view state (commit message, collapsed sections, graph).
    pub(super) fn select_repo(&mut self, root: &Path) {
        if self.repo.as_ref().is_some_and(|r| r.root == root) {
            return;
        }
        let Some(i) = self.parked_repos.iter().position(|p| p.repo.root == root) else { return };
        let next = self.parked_repos.remove(i);
        let (clone, askpass) = (self.git.clone.take(), self.git.askpass.take());
        if let Some(repo) = self.repo.take() {
            let (open, height) = (self.scm_graph.open, self.scm_graph.height);
            let parked = ParkedRepo { repo, scm: std::mem::take(&mut self.scm), graph: std::mem::take(&mut self.scm_graph), git: std::mem::take(&mut self.git), pending: Vec::new() };
            self.parked_repos.push(parked);
            (self.scm_graph.open, self.scm_graph.height) = (open, height);
        }
        let (open, height) = (self.scm_graph.open, self.scm_graph.height);
        self.repo = Some(next.repo);
        self.scm = next.scm;
        self.scm_graph = next.graph;
        (self.scm_graph.open, self.scm_graph.height) = (open, height);
        self.git = next.git;
        (self.git.clone, self.git.askpass) = (clone, askpass);
        self.git_branch = self.repo.as_ref().and_then(|r| super::read_git_branch(&r.root));
        self.handle_scm_events(next.pending);
        self.refresh_scm();
        self.refresh_timeline();
        self.refresh_scm_graph();
    }

    /// The repository `path` is in (the innermost).
    pub(super) fn repo_root_of(&self, path: &Path) -> Option<PathBuf> {
        self.repo_roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.components().count()).cloned()
    }

    /// With several repositories, the Source Control view follows the active editor's, like
    /// the status bar (a repository picked in the list stays until the editor changes).
    fn follow_active_editor_repo(&mut self) {
        if self.repo_roots.len() < 2 || self.repo.as_ref().is_some_and(|r| r.busy().is_some()) {
            return;
        }
        let Some(path) = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        if self.repo_followed.as_ref() == Some(&path) {
            return;
        }
        self.repo_followed = Some(path.clone());
        if let Some(root) = self.repo_root_of(&path) {
            self.select_repo(&root);
        }
    }

    /// Asks git for a fresh status (after saves, on focus, periodically).
    pub(super) fn refresh_scm(&mut self) {
        if let Some(repo) = &self.repo {
            repo.send(Job::Refresh);
            self.scm.last_refresh = Some(Instant::now());
        }
    }

    pub(super) fn scm_deadline(&self) -> Option<Instant> {
        self.repo.as_ref()?;
        self.scm.last_refresh.map(|t| t + POLL)
    }

    /// Applies worker events and polls periodically. Called every frame.
    pub(super) fn scm_tick(&mut self) {
        self.follow_active_editor_repo();
        if self.scm.last_refresh.is_some_and(|t| t.elapsed() >= POLL) {
            self.refresh_scm();
        }
        self.git_tick();
        self.timeline_tick();
        self.scm_graph_tick();
        // Parked repositories keep their status fresh (for the Explorer's colors and the
        // Repositories list); anything else they report waits until they're shown.
        for p in &mut self.parked_repos {
            if p.scm.last_refresh.is_none_or(|t| t.elapsed() >= POLL) {
                p.repo.send(Job::Refresh);
                p.scm.last_refresh = Some(Instant::now());
            }
            p.pending.extend(p.repo.poll().into_iter().filter(|e| !matches!(e, Event::Status(_))));
        }
        let Some(repo) = &mut self.repo else { return };
        let events = repo.poll();
        self.handle_scm_events(events);
    }

    fn handle_scm_events(&mut self, events: Vec<Event>) {
        let mut status_changed = false;
        for event in events {
            match event {
                Event::Status(_) => status_changed = true,
                Event::Done { op, output } => self.git_done(op, output),
                Event::Failed { op, error } => self.git_failed(op, error),
                Event::FileLog(path, entries) => self.timeline_arrived(path, entries),
                Event::Graph(commits) => self.scm_graph_arrived(commits),
                Event::CommitFiles(hash, files) => self.commit_files_arrived(hash, files),
                Event::HeadContents(path, text) => {
                    self.scm.head_requested.remove(&path);
                    self.scm.head_cache.insert(path.clone(), text.map(Arc::new));
                    self.scm.marks.remove(&path);
                }
                Event::Error(e) => {
                    let msg = e.lines().take(12).collect::<Vec<_>>().join("\n");
                    self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Git").set_description(msg).show();
                }
            }
        }
        if status_changed {
            self.refresh_timeline();
            self.refresh_scm_graph();
            // A new HEAD (commit, checkout, pull) invalidates the cached HEAD contents.
            let head = self.repo.as_ref().and_then(|r| r.status.head.clone());
            if head != self.scm.cached_head {
                self.scm.cached_head = head;
                self.scm.head_cache.clear();
                self.scm.marks.clear();
            }
            self.reload_clean_docs();
            self.reload_diffs();
        }
    }

    /// Opens a side-by-side diff for a changed file in the active editor group.
    pub(super) fn open_diff(&mut self, spec: DiffSpec) {
        // Git diffs need the repository; file comparisons don't.
        let root = match (self.repo.as_ref(), &spec.left_file) {
            (Some(r), _) => r.root.clone(),
            (None, Some(_)) => PathBuf::from("/"),
            (None, None) => return,
        };
        let lang = language::Lang::detect(Some(&spec.path));
        let state = DiffState::load(&root, spec.clone(), lang);
        self.open_diff_state(state);
    }

    /// Shows `left` ↔ `right` for `path` in a diff tab (`what` names the right side).
    pub(super) fn open_text_diff(&mut self, path: &Path, left: &str, right: &str, what: &str) {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let lang = language::Lang::detect(Some(path));
        // A revision that isn't one: such diffs are never reloaded from git.
        let spec = DiffSpec { path: path.to_path_buf(), staged: false, revision: Some(format!(":{what}")), left_file: None };
        let state = DiffState::fixed(spec, format!("{name} ↔ {name} ({what})"), left, right, lang);
        self.open_diff_state(state);
    }

    fn open_diff_state(&mut self, state: DiffState) {
        let spec = state.spec.clone();
        let name = spec.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        // The tab needs a document: the file itself, or a placeholder if it no longer exists.
        let doc = if spec.path.is_file() {
            match self.doc_for_path(&spec.path) {
                Some(d) => d,
                None => return,
            }
        } else {
            self.add_doc(Doc::virtual_named(&name))
        };
        let g = self.active_group;
        let group = &mut self.groups[g];
        match group.tabs.iter().position(|t| t.diff.as_ref().is_some_and(|d| d.spec == spec)) {
            Some(i) => {
                group.tabs[i].diff = Some(Box::new(state));
                group.active = i;
            }
            None => {
                // Opened from the Source Control view as a preview.
                let mut ed = EditorState::new(doc);
                ed.diff = Some(Box::new(state));
                ed.preview = self.settings.bool("workbench.editor.enablePreview");
                match group.tabs.iter().position(|t| t.preview).filter(|_| ed.preview) {
                    Some(i) => {
                        group.tabs[i] = ed;
                        group.active = i;
                    }
                    None => {
                        let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
                        group.tabs.insert(at, ed);
                        group.active = at;
                    }
                }
            }
        }
        self.focus = Focus::Editor;
    }

    /// Reloads open diff tabs after git state changed (stage, commit, checkout).
    fn reload_diffs(&mut self) {
        let Some(root) = self.repo.as_ref().map(|r| r.root.clone()) else { return };
        for group in &mut self.groups {
            for tab in &mut group.tabs {
                let Some(diff) = tab.diff.as_mut().filter(|d| d.spec.revision.is_none() && d.spec.left_file.is_none()) else { continue };
                let lang = language::Lang::detect(Some(&diff.spec.path));
                let mut fresh = DiffState::load(&root, diff.spec.clone(), lang);
                fresh.scroll_y = diff.scroll_y;
                fresh.scroll_x = diff.scroll_x;
                **diff = fresh;
            }
        }
    }

    /// "Git: Open Changes" for the active file.
    pub(super) fn open_changes_for_active(&mut self) {
        let Some(path) = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        let Some(repo) = &self.repo else { return };
        let unstaged = repo.status.unstaged.iter().any(|c| c.path == path && c.status != FileStatus::Untracked);
        let staged = repo.status.staged.iter().any(|c| c.path == path);
        if unstaged || staged {
            self.open_diff(DiffSpec { path, staged: staged && !unstaged, revision: None, left_file: None });
        }
    }

    /// Keys in a diff tab: scrolling and change navigation (the view is read-only).
    pub(super) fn diff_key(&mut self, k: &KeyInput) {
        if let Some(cmd) = k.command() {
            return self.run(cmd);
        }
        let g = self.active_group;
        let group = &mut self.groups[g];
        let Some(diff) = group.tabs.get_mut(group.active).and_then(|t| t.diff.as_mut()) else { return };
        let page = (diff.view.h / crate::editor::line_height()).floor() * crate::editor::line_height();
        let line = crate::editor::line_height();
        match k.key {
            Key::Up => diff.scroll_by(0.0, line),
            Key::Down => diff.scroll_by(0.0, -line),
            Key::PageUp => diff.scroll_by(0.0, page),
            Key::PageDown | Key::Space => diff.scroll_by(0.0, -page),
            Key::Home => diff.scroll_y = 0.0,
            Key::End => diff.scroll_by(0.0, -1e9),
            _ => {}
        }
    }

    /// Next/previous difference in the active diff tab (F7 / ⇧F7).
    pub(super) fn diff_step(&mut self, forward: bool) {
        let g = self.active_group;
        let group = &mut self.groups[g];
        let Some(diff) = group.tabs.get_mut(group.active).and_then(|t| t.diff.as_mut()) else { return };
        if let Some(row) = diff.next_change(forward) {
            diff.scroll_to_row(row);
        }
    }

    /// Reloads open documents without unsaved changes whose file changed on disk
    /// (after discard, checkout, or edits made outside the editor).
    fn reload_clean_docs(&mut self) {
        for doc in self.docs.iter_mut().flatten() {
            if doc.buffer.is_dirty() {
                continue;
            }
            let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { continue };
            let Ok(disk) = std::fs::read_to_string(&path) else { continue };
            let disk = crate::search_editor::document_text(doc.lang, disk);
            if disk != doc.buffer.text() {
                let all = text::Selection { anchor: text::Pos::new(0, 0), head: doc.buffer.end(), goal_col: None };
                doc.buffer.insert(all, &disk);
                doc.buffer.mark_saved();
            }
        }
    }

    /// Gutter change markers for a document, computing (or requesting HEAD contents) as needed.
    pub(super) fn git_marks(&mut self, doc_id: usize) -> Vec<LineChange> {
        if !crate::config::get().scm_gutter {
            return Vec::new();
        }
        let Some(repo) = &self.repo else { return Vec::new() };
        let Some(doc) = self.docs.get(doc_id).and_then(Option::as_ref).filter(|d| !d.large) else { return Vec::new() };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return Vec::new() };
        if !path.starts_with(&repo.root) {
            return Vec::new();
        }
        let version = doc.buffer.version();
        if let Some((v, marks)) = self.scm.marks.get(&path) {
            if *v == version {
                return marks.clone();
            }
        }
        match self.scm.head_cache.get(&path) {
            None => {
                if self.scm.head_requested.insert(path.clone()) {
                    repo.send(Job::HeadContents(path));
                }
                Vec::new()
            }
            Some(None) => Vec::new(), // not tracked: no markers
            Some(Some(head)) => {
                let marks = scm::line_changes(head, &doc.buffer.text());
                self.scm.marks.insert(path, (version, marks.clone()));
                marks
            }
        }
    }

    /// Git status per path for decorations, plus folders containing changes.
    pub(super) fn git_decorations(&self) -> (HashMap<PathBuf, FileStatus>, HashSet<PathBuf>) {
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        for repo in self.repo.iter().chain(self.parked_repos.iter().map(|p| &p.repo)) {
            let st = &repo.status;
            // Staged first so working-tree status (usually more relevant) wins.
            for c in st.staged.iter().chain(&st.unstaged).chain(&st.conflicts) {
                files.insert(c.path.clone(), c.status);
                let mut p = c.path.parent();
                while let Some(dir) = p {
                    if !dir.starts_with(&repo.root) || !dirs.insert(dir.to_path_buf()) {
                        break;
                    }
                    p = dir.parent();
                }
            }
        }
        (files, dirs)
    }

    pub(super) fn focus_scm(&mut self) {
        self.view = View::Scm;
        self.sidebar_visible = true;
        if self.repo.is_some() {
            self.focus = Focus::Scm;
        }
    }

    pub(super) fn scm_key(&mut self, k: &KeyInput) {
        match k.key {
            Key::Enter if k.cmd => return self.scm_action(ScmAction::Commit),
            Key::Escape => {
                self.focus = Focus::Editor;
                return;
            }
            _ => {}
        }
        if self.scm.message.key(k) == FieldEvent::Ignored {
            if let Some(cmd) = k.command() {
                self.run(cmd);
            }
        }
    }

    pub(super) fn scm_clipboard(&mut self, cut: bool, paste: bool, select_all: bool) {
        let f = &mut self.scm.message;
        if select_all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.scm.message.insert(&text);
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn click_scm_message(&mut self, x: f32, shift: bool) {
        self.focus = Focus::Scm;
        self.scm.message.click(x, shift);
    }

    pub(super) fn scroll_scm(&mut self, dy: f32) {
        self.scm.scroll = (self.scm.scroll - dy).max(0.0);
    }

    fn section_changes(&self, section: Section) -> Vec<Change> {
        let Some(repo) = &self.repo else { return Vec::new() };
        match section {
            Section::Merge => repo.status.conflicts.clone(),
            Section::Staged => repo.status.staged.clone(),
            Section::Changes => repo.status.unstaged.clone(),
        }
    }

    pub(super) fn confirm(&self, title: &str, detail: &str, button: &str) -> bool {
        let answer = self
            .message_dialog()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(title)
            .set_description(detail)
            .set_buttons(rfd::MessageButtons::OkCancelCustom(button.into(), "Cancel".into()))
            .show();
        matches!(answer, rfd::MessageDialogResult::Custom(ref s) if s == button) || answer == rfd::MessageDialogResult::Ok
    }

    /// Discards working-tree changes: restores tracked files, deletes untracked ones.
    fn discard(&mut self, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        let untracked: Vec<&Change> = changes.iter().filter(|c| c.status == FileStatus::Untracked).collect();
        let name = |c: &Change| c.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let ok = if changes.len() == 1 && untracked.len() == 1 {
            self.confirm(
                &format!("Are you sure you want to DELETE '{}'?", name(&changes[0])),
                "This is IRREVERSIBLE! This file will be FOREVER LOST if you proceed.",
                "Delete File",
            )
        } else if changes.len() == 1 {
            self.confirm(
                &format!("Are you sure you want to discard changes in '{}'?", name(&changes[0])),
                "This can't be undone.",
                "Discard File",
            )
        } else {
            let detail = if untracked.is_empty() {
                "This can't be undone.".to_string()
            } else {
                format!("This will DELETE {} untracked file(s). This is IRREVERSIBLE!", untracked.len())
            };
            self.confirm(&format!("Are you sure you want to discard ALL {} changes?", changes.len()), &detail, "Discard All")
        };
        if !ok {
            return;
        }
        let mut failed = Vec::new();
        for c in &changes {
            if c.status == FileStatus::Untracked {
                if let Err(e) = std::fs::remove_file(&c.path) {
                    failed.push(format!("{}: {e}", c.path.display()));
                }
            }
        }
        let tracked: Vec<PathBuf> =
            changes.iter().filter(|c| c.status != FileStatus::Untracked).map(|c| c.path.clone()).collect();
        if let Some(repo) = &mut self.repo {
            if tracked.is_empty() {
                repo.send(Job::Refresh);
            } else {
                repo.run(Op::Discard(tracked));
            }
        }
        if !failed.is_empty() {
            self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Delete failed").set_description(failed.join("\n")).show();
        }
    }

    /// Commits (or amends) with the message in the box, then runs `after` (push or sync).
    pub(super) fn commit_with(&mut self, amend: bool, after: super::git_actions::AfterCommit) {
        use super::git_actions::AfterCommit;
        let Some(repo) = &self.repo else { return };
        let st = &repo.status;
        let message = self.scm.message.text.trim().to_string();
        if st.change_count() == 0 && !st.merging && !amend {
            return;
        }
        if message.is_empty() && !amend {
            self.message_dialog().set_title("Please provide a commit message.").show();
            self.focus = Focus::Scm;
            return;
        }
        let mut all = false;
        if st.staged.is_empty() && st.change_count() > 0 && !(amend && st.unstaged.is_empty() && st.conflicts.is_empty()) {
            // The "smart commit" prompt.
            if !self.settings.bool("git.enableSmartCommit") {
                let answer = self
                    .message_dialog()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("There are no staged changes to commit.")
                    .set_description("Would you like to stage all your changes and commit them directly?")
                    .set_buttons(rfd::MessageButtons::YesNoCancelCustom("Yes".into(), "Always".into(), "Cancel".into()))
                    .show();
                match answer {
                    rfd::MessageDialogResult::Custom(ref s) if s == "Yes" => {}
                    rfd::MessageDialogResult::Custom(ref s) if s == "Always" => {
                        self.update_setting(settings::Scope::User, "git.enableSmartCommit", Some(serde_json::Value::Bool(true)));
                    }
                    _ => return,
                }
            }
            all = true;
        }
        // An explicit Commit & Push/Sync wins over `git.postCommitCommand`.
        let after = match (after, self.settings.string("git.postCommitCommand").as_str()) {
            (AfterCommit::Nothing, "push") => AfterCommit::Push,
            (AfterCommit::Nothing, "sync") => AfterCommit::Sync,
            (after, _) => after,
        };
        self.git.after_commit = Some(after);
        if let Some(repo) = &mut self.repo {
            repo.run(Op::Commit { message, all, amend });
        }
    }

    fn init_repo(&mut self) {
        // The first folder without a repository.
        let Some(root) = self.folders().into_iter().find(|f| self.repo_root_of(f).is_none()) else { return };
        let out = std::process::Command::new("git").arg("-C").arg(&root).args(["init", "-q"]).output();
        match out {
            Ok(o) if o.status.success() => self.open_repo(&root),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("git init failed").set_description(err).show();
            }
            Err(e) => {
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Could not run git").set_description(e.to_string()).show();
            }
        }
    }

    pub(super) fn scm_action(&mut self, action: ScmAction) {
        let send = |wb: &mut Self, op: Op| {
            if let Some(repo) = &mut wb.repo {
                repo.run(op);
            }
        };
        match action {
            ScmAction::Commit => self.commit_with(false, super::git_actions::AfterCommit::Nothing),
            ScmAction::CommitMenu => {
                let r = self.scm.commit_menu_rect;
                let entries = self.commit_menu_entries();
                self.show_popup(entries, r.x, r.bottom() + 2.0);
            }
            ScmAction::More => {
                let r = self.scm.more_rect;
                let entries = self.more_actions_entries();
                self.show_popup(entries, r.x, r.bottom() + 2.0);
            }
            ScmAction::Publish => self.run(crate::commands::Command::GitPublish),
            ScmAction::Sync => self.run(crate::commands::Command::GitSync),
            ScmAction::ContinueRebase => self.run(crate::commands::Command::GitContinueRebase),
            ScmAction::Refresh => self.refresh_scm(),
            ScmAction::InitRepo => self.init_repo(),
            ScmAction::InstallGit => {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
                if let Err(e) = self.run_task_terminal("Install the Command Line Tools", "xcode-select --install", &home, &[]) {
                    self.set_status_message(&e);
                }
            }
            ScmAction::StageAll => {
                let paths: Vec<PathBuf> = self.section_changes(Section::Changes).into_iter().map(|c| c.path).collect();
                if !paths.is_empty() {
                    send(self, Op::Stage(paths));
                }
            }
            ScmAction::UnstageAll => {
                let paths: Vec<PathBuf> = self.section_changes(Section::Staged).into_iter().map(|c| c.path).collect();
                if !paths.is_empty() {
                    send(self, Op::Unstage(paths));
                }
            }
            ScmAction::DiscardAll => {
                let changes = self.section_changes(Section::Changes);
                self.discard(changes);
            }
            ScmAction::ToggleSection(s) => {
                if !self.scm.collapsed.remove(&s) {
                    self.scm.collapsed.insert(s);
                }
            }
            ScmAction::Open(section, i) => {
                if let Some(c) = self.section_changes(section).get(i) {
                    if c.path.is_file() {
                        let path = c.path.clone();
                        self.open_file(&path);
                        self.focus = Focus::Editor;
                    }
                }
            }
            ScmAction::Stage(section, i) => {
                if let Some(c) = self.section_changes(section).get(i).cloned() {
                    // Staging marks a conflict resolved: check the markers are gone first.
                    let markers = section == Section::Merge
                        && std::fs::read_to_string(&c.path).is_ok_and(|t| crate::conflicts::has_conflicts(&t));
                    if markers {
                        let name = c.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        let title = format!("Are you sure you want to stage {name} with merge conflicts?");
                        if !self.confirm(&title, "", "Stage") {
                            return;
                        }
                    }
                    send(self, Op::Stage(vec![c.path]));
                }
            }
            ScmAction::Unstage(i) => {
                if let Some(c) = self.section_changes(Section::Staged).get(i) {
                    let mut paths = vec![c.path.clone()];
                    paths.extend(c.orig_path.clone());
                    send(self, Op::Unstage(paths));
                }
            }
            ScmAction::Discard(section, i) => {
                if let Some(c) = self.section_changes(section).get(i).cloned() {
                    self.discard(vec![c]);
                }
            }
        }
    }

    // ------------------------------------------------------------------ drawing

    fn scm_button(&mut self, c: &mut Canvas, r: Rect, icon: &Icon, action: ScmAction) {
        let hit = Hit::Scm(action);
        if self.hovered(hit) {
            c.fill_rounded(r, self.color("inputOption.hoverBackground"), 3.0);
        }
        c.icon_in(icon, r, 16.0, self.color("icon.foreground"));
        self.hits.push((r, hit));
    }

    /// Header actions (commit, refresh) for the view title.
    pub(super) fn draw_scm_header_actions(&mut self, c: &mut Canvas, header: Rect) {
        if self.repo.is_none() {
            return;
        }
        let b = |i: f32| Rect::new(header.right() - 32.0 - i * 26.0, header.y + 6.0, 24.0, 22.0);
        self.scm.more_rect = b(0.0);
        self.scm_button(c, b(0.0), &icons::ELLIPSIS, ScmAction::More);
        self.scm_button(c, b(1.0), &icons::REFRESH, ScmAction::Refresh);
        self.scm_button(c, b(2.0), &icons::CHECK, ScmAction::Commit);
    }

    /// The repositories of a multi-root workspace, above the shown one's changes (the
    /// Source Control Repositories view). Returns the height used.
    fn draw_scm_repositories(&mut self, c: &mut Canvas, body: Rect) -> f32 {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let dim = self.color("descriptionForeground");
        let active = self.repo.as_ref().map(|r| r.root.clone());
        let mut y = body.y;
        c.text_in(Rect::new(body.x + 12.0, y, body.w - 24.0, ROW_H), "REPOSITORIES", &TextStyle::ui(SMALL, fg).weight(700));
        y += ROW_H;
        let repos: Vec<(PathBuf, Option<String>)> = self
            .repo_roots
            .iter()
            .map(|root| {
                let st = if active.as_ref() == Some(root) {
                    self.repo.as_ref().map(|r| &r.status)
                } else {
                    self.parked_repos.iter().find(|p| p.repo.root == *root).map(|p| &p.repo.status)
                };
                (root.clone(), st.and_then(|st| st.branch.clone().or_else(|| st.detached_at.clone())))
            })
            .collect();
        for (i, (root, branch)) in repos.iter().enumerate() {
            let rr = Rect::new(body.x, y, body.w, ROW_H);
            let hit = Hit::ScmRepo(i);
            if active.as_ref() == Some(root) {
                c.fill_rounded(super::row_pill(rr), self.color(if self.focus == Focus::Scm { "list.activeSelectionBackground" } else { "list.inactiveSelectionBackground" }), super::ROW_RADIUS);
            } else if self.hovered(hit) {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            c.icon(&icons::SOURCE_CONTROL, rr.x + 20.0, y + 3.0, 16.0, fg);
            let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
            let st = TextStyle::ui(UI, fg).weight(600);
            c.text_in(Rect::new(rr.x + 42.0, y, rr.w - 50.0, ROW_H), &name, &st);
            if let Some(b) = branch {
                let bs = TextStyle::ui(UI, dim);
                let bw = c.measure(b, &bs);
                let bx = (rr.right() - 12.0 - bw).max(rr.x + 50.0 + c.measure(&name, &st));
                c.icon(&icons::BRANCH, bx - 18.0, y + 3.0, 16.0, dim);
                c.text_in(Rect::new(bx, y, rr.right() - 12.0 - bx, ROW_H), b, &bs);
            }
            self.hits.push((rr, hit));
            y += ROW_H;
        }
        c.fill(Rect::new(body.x, y + 3.0, body.w, 1.0), self.color_or("sideBarSectionHeader.border", "widget.border"));
        y + 8.0 - body.y
    }

    /// A click in the Repositories list.
    pub(super) fn click_scm_repo(&mut self, i: usize) {
        self.focus = Focus::Scm;
        if let Some(root) = self.repo_roots.get(i).cloned() {
            self.select_repo(&root);
        }
    }

    pub(super) fn draw_scm_view(&mut self, c: &mut Canvas, body: Rect) {
        let body = if self.repo_roots.len() > 1 {
            let used = self.draw_scm_repositories(c, body);
            Rect::new(body.x, body.y + used, body.w, (body.h - used).max(0.0))
        } else {
            body
        };
        let fg = self.color_or("sideBar.foreground", "foreground");
        let x = body.x + 12.0;
        let w = body.w - 24.0;
        let Some(repo) = &self.repo else {
            // No git at all (no Command Line Tools): say so, and offer to install them.
            if self.tree.is_some() && scm::git_available().is_err() {
                let detail = "macOS installs it with the Command Line Tools (or install it with Homebrew: brew install git), then reopen the folder.";
                let action = Some(("Install the Command Line Tools", Hit::Scm(ScmAction::InstallGit)));
                self.empty_state(c, body, &icons::SOURCE_CONTROL, "Source control needs git", detail, action);
            } else if self.tree.is_some() {
                let detail = "This folder isn't a git repository yet. Initialize one to track its changes.";
                self.empty_state(c, body, &icons::SOURCE_CONTROL, "No repository", detail, Some(("Initialize Repository", Hit::Scm(ScmAction::InitRepo))));
            } else {
                self.empty_state(c, body, &icons::SOURCE_CONTROL, "No folder open", "Open a folder to use source control.", None);
            }
            return;
        };
        let branch = repo.status.branch.clone().or_else(|| repo.status.detached_at.clone()).unwrap_or_else(|| "main".into());
        let has_changes = repo.status.change_count() > 0;
        let (merge, staged, changes) = (repo.status.conflicts.clone(), repo.status.staged.clone(), repo.status.unstaged.clone());

        // Commit message and button.
        let mut y = body.y + 4.0;
        let input = Rect::new(x, y, w, FIELD_H);
        let focused = self.focus == Focus::Scm && self.palette.is_none();
        self.field_frame(c, input, focused);
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let (ph, sel, caret_on) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"), self.caret_on());
        let placeholder = format!("Message (⌘⏎ to commit on '{branch}')");
        self.scm.message.draw(c, Self::field_text_rect(input), &style, &placeholder, ph, focused, caret_on, sel);
        self.hits.push((input, Hit::ScmMessage));
        y += FIELD_H + 6.0;
        // The action button: Commit, or Publish Branch / Sync Changes when there's nothing to
        // commit, or Continue during a rebase.
        let st = &repo.status;
        let busy = repo.busy().is_some();
        let committable = has_changes || st.merging;
        let sync_label = format!("Sync Changes {}↓ {}↑", st.behind, st.ahead);
        let (label, icon, action, enabled, chevron) = if st.rebasing {
            ("Continue".to_string(), &icons::CHECK, ScmAction::ContinueRebase, true, false)
        } else if committable {
            ("Commit".to_string(), &icons::CHECK, ScmAction::Commit, true, true)
        } else if st.branch.is_some() && st.upstream.is_none() {
            ("Publish Branch".to_string(), &icons::CLOUD_UPLOAD, ScmAction::Publish, true, false)
        } else if st.upstream.is_some() && (st.ahead > 0 || st.behind > 0) {
            (sync_label, &icons::SYNC, ScmAction::Sync, true, false)
        } else {
            ("Commit".to_string(), &icons::CHECK, ScmAction::Commit, false, false)
        };
        let enabled = enabled && !busy;
        let btn = Rect::new(x, y, w, FIELD_H);
        let (main, menu) = if chevron { btn.cut_right(FIELD_H) } else { (btn, Rect::default()) };
        let hit = Hit::Scm(action);
        let menu_hit = Hit::Scm(ScmAction::CommitMenu);
        let bg = |wb: &Self, h: Hit| {
            if !enabled {
                wb.color("input.background")
            } else if wb.hovered(h) {
                wb.color("button.hoverBackground")
            } else {
                wb.color("button.background")
            }
        };
        c.fill_rounded(btn, bg(self, hit), FIELD_RADIUS);
        let bfg = if enabled { self.color("button.foreground") } else { self.color("descriptionForeground") };
        if chevron {
            if enabled && self.hovered(menu_hit) {
                c.fill_rounded(menu, bg(self, menu_hit), FIELD_RADIUS);
            }
            c.fill(Rect::new(menu.x, menu.y + 4.0, 1.0, menu.h - 8.0), self.color_or("button.separator", "button.foreground"));
            c.icon_in(&icons::CHEVRON_DOWN, menu, 16.0, bfg);
            self.scm.commit_menu_rect = menu;
        }
        let bs = TextStyle::ui(UI, bfg);
        let tw = c.measure(&label, &bs) + 20.0;
        let bx = main.x + (main.w - tw) / 2.0;
        let turn = if self.git_spinning() && action == ScmAction::Sync { self.git_spin_turn() } else { 0 };
        c.icon_turned(icon, bx, btn.y + (btn.h - 16.0) / 2.0, 16.0, bfg, turn);
        c.text_in(Rect::new(bx + 20.0, btn.y, tw, btn.h), &label, &bs);
        if enabled {
            self.hits.push((main, hit));
            if chevron {
                self.hits.push((menu, menu_hit));
            }
        }
        y += FIELD_H + 8.0;

        // Change lists, and the GRAPH section below them.
        let avail = (body.bottom() - y).max(0.0);
        let graph_h = self.scm_graph_height(avail);
        let list = Rect::new(body.x, y, body.w, (avail - ROW_H - graph_h).max(0.0));
        let mut rows: Vec<(Section, Option<usize>)> = Vec::new();
        for (section, items) in [(Section::Merge, &merge), (Section::Staged, &staged), (Section::Changes, &changes)] {
            if items.is_empty() && section != Section::Changes {
                continue;
            }
            rows.push((section, None));
            if !self.scm.collapsed.contains(&section) {
                rows.extend((0..items.len()).map(|i| (section, Some(i))));
            }
        }
        let max = (rows.len() as f32 * ROW_H - list.h + ROW_H).max(0.0);
        self.scm.scroll = self.scm.scroll.min(max);
        let first = (self.scm.scroll / ROW_H) as usize;
        let visible = (list.h / ROW_H).ceil() as usize + 1;
        let root = repo.root.clone();
        let dim = self.color("descriptionForeground");
        let small = TextStyle::ui(12.0, dim);
        c.push_clip(list);
        self.scm.rows.clear();
        for (row, &(section, item)) in rows.iter().enumerate().skip(first).take(visible) {
            let ry = list.y + row as f32 * ROW_H - self.scm.scroll;
            let rr = Rect::new(list.x, ry, list.w, ROW_H);
            let row_hit = Hit::ScmRow(row);
            // Row actions show while the pointer is anywhere on the row (including its buttons).
            let hovered = self.drag.is_none() && list.contains(self.mouse.0, self.mouse.1) && rr.contains(self.mouse.0, self.mouse.1);
            if hovered {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let items = match section {
                Section::Merge => &merge,
                Section::Staged => &staged,
                Section::Changes => &changes,
            };
            match item {
                None => {
                    let collapsed = self.scm.collapsed.contains(&section);
                    c.icon(if collapsed { &icons::CHEVRON_RIGHT } else { &icons::CHEVRON_DOWN }, rr.x + 8.0, ry + 3.0, 16.0, fg);
                    let label = match section {
                        Section::Merge => "Merge Changes",
                        Section::Staged => "Staged Changes",
                        Section::Changes => "Changes",
                    };
                    c.text_in(Rect::new(rr.x + 26.0, ry, rr.w, ROW_H), label, &TextStyle::ui(SMALL, fg).weight(700));
                    let n = items.len();
                    let bw = badge_w(c, n);
                    self.badge(c, rr.right() - 12.0 - bw, ry + 3.0, n);
                    self.hits.push((rr, Hit::Scm(ScmAction::ToggleSection(section))));
                    if hovered {
                        let b = |i: f32| Rect::new(rr.right() - 16.0 - bw - (i + 1.0) * 22.0, ry + 1.0, 20.0, 20.0);
                        match section {
                            Section::Staged => self.scm_button(c, b(0.0), &icons::REMOVE, ScmAction::UnstageAll),
                            Section::Changes if n > 0 => {
                                self.scm_button(c, b(0.0), &icons::ADD, ScmAction::StageAll);
                                self.scm_button(c, b(1.0), &icons::DISCARD, ScmAction::DiscardAll);
                            }
                            _ => {}
                        }
                    }
                }
                Some(i) => {
                    let change = &items[i];
                    let color = status_color(&self.theme, change.status);
                    c.icon(&icons::FILE, rr.x + 26.0, ry + 3.0, 16.0, super::file_color(&change.path));
                    let name = change.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let dir = change.path.parent().and_then(|p| p.strip_prefix(&root).ok()).map(|p| p.display().to_string()).unwrap_or_default();
                    let actions_w = if hovered { 70.0 } else { 0.0 };
                    c.push_clip(Rect::new(rr.x, ry, rr.w - 28.0 - actions_w, ROW_H));
                    let name_style = TextStyle::ui(UI, color);
                    let nw = c.text_in(Rect::new(rr.x + 48.0, ry, rr.w, ROW_H), &name, &name_style);
                    if change.status == FileStatus::Deleted {
                        c.fill(Rect::new(rr.x + 48.0, ry + ROW_H / 2.0, nw, 1.0), color);
                    }
                    c.text_in(Rect::new(rr.x + 54.0 + nw, ry, rr.w, ROW_H), &dir, &small);
                    c.pop_clip();
                    let letter = change.status.letter().to_string();
                    c.text_in(Rect::new(rr.right() - 22.0, ry, 14.0, ROW_H), &letter, &TextStyle::ui(UI, color));
                    self.hits.push((rr, row_hit));
                    self.scm.rows.push((row, section, i));
                    if hovered {
                        let b = |k: f32| Rect::new(rr.right() - 26.0 - (k + 1.0) * 22.0, ry + 1.0, 20.0, 20.0);
                        self.scm_button(c, b(2.0), &icons::GO_TO_FILE, ScmAction::Open(section, i));
                        match section {
                            Section::Staged => self.scm_button(c, b(0.0), &icons::REMOVE, ScmAction::Unstage(i)),
                            _ => {
                                self.scm_button(c, b(1.0), &icons::DISCARD, ScmAction::Discard(section, i));
                                self.scm_button(c, b(0.0), &icons::ADD, ScmAction::Stage(section, i));
                            }
                        }
                    }
                }
            }
        }
        c.pop_clip();

        let head = Rect::new(body.x, list.bottom(), body.w, ROW_H);
        self.section_header(c, head, "GRAPH", self.scm_graph.open, Hit::ScmGraphSection);
        if self.scm_graph.open {
            self.draw_scm_graph(c, Rect::new(body.x, head.bottom(), body.w, graph_h));
            let sash = Rect::new(body.x, head.y - 2.0, body.w, 4.0);
            self.hits.push((sash, Hit::ScmGraphSash));
            if matches!(self.drag, Some(super::Drag::ScmGraphSash)) || (self.drag.is_none() && self.hover_hit == Some(Hit::ScmGraphSash)) {
                c.fill(sash, self.color("sash.hoverBorder"));
            }
        }

        // An indeterminate progress bar while git talks to a remote.
        if self.git_spinning() {
            let t = (self.git_spin_turn() % 40) as f32 / 40.0;
            let seg = body.w * 0.3;
            let bx = body.x - seg + t * (body.w + seg);
            c.push_clip(Rect::new(body.x, body.y, body.w, 2.0));
            c.fill(Rect::new(bx, body.y, seg, 2.0), self.color("progressBar.background"));
            c.pop_clip();
        }
    }

    /// Clicking a change row opens its diff (untracked and conflicted files open
    /// directly).
    pub(super) fn click_scm_row(&mut self, row: usize) {
        let Some(&(_, section, i)) = self.scm.rows.iter().find(|(r, ..)| *r == row) else { return };
        let Some(change) = self.section_changes(section).get(i).cloned() else { return };
        match (section, change.status) {
            (Section::Merge, _) if self.settings.bool("git.mergeEditor") && change.path.is_file() => self.open_merge_editor(&change.path),
            (Section::Merge, _) | (_, FileStatus::Untracked) => self.scm_action(ScmAction::Open(section, i)),
            _ => self.open_diff(DiffSpec { path: change.path, staged: section == Section::Staged, revision: None, left_file: None }),
        }
    }
}

fn badge_w(c: &mut Canvas, n: usize) -> f32 {
    (c.measure(&n.to_string(), &TextStyle::ui(SMALL, Color::TRANSPARENT)) + 10.0).max(18.0)
}
