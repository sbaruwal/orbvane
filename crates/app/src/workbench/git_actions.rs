//! The git workflow beyond staging and committing: branches, merge and rebase, fetch / pull /
//! push / sync, stashes, remotes, tags, clone, and resolving merge conflicts in the editor.
//! Choices use the quick input (pickers and input boxes, worded), operations
//! run on the repository's worker, and failures that have a way out offer it (Stash &
//! Checkout, Pull before pushing, force-delete an unmerged branch).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use scm::{Op, Ref, RefKind};
use serde_json::Value;
use settings::Scope;

use super::{Focus, PopupItem, View, Workbench};
use crate::commands::Command;
use crate::conflicts::{ConflictCache, Resolution};
use crate::palette::{Action, InputBox, Item, Palette, Picker};
use crate::workbench::preferences::PopupAction;

/// A choice in one of the git pickers.
#[derive(Clone, Debug)]
pub enum GitPick {
    Checkout(String),
    CheckoutDetached(String),
    /// "+ Create new branch..."
    NewBranch,
    /// "+ Create new branch from..."
    NewBranchFrom,
    /// "Checkout detached..."
    DetachedPicker,
    /// The ref to create a branch from was picked; ask for the name next.
    BranchFrom(String),
    DeleteBranch(String),
    Merge(String),
    Rebase(String),
    Publish(String),
    StashPop(usize),
    StashApply(usize),
    StashDrop(usize),
    RemoveRemote(String),
    DeleteTag(String),
}

/// What an input box asks for.
#[derive(Clone, Debug)]
pub enum GitInput {
    NewBranch { from: Option<String> },
    RenameBranch,
    StashMessage { untracked: bool, staged: bool },
    RemoteUrl,
    RemoteName { url: String },
    TagName,
    TagMessage { name: String },
    CloneUrl,
    /// A username, password, passphrase or confirmation git or ssh asked for.
    Credential,
    /// A key being recorded for a command (Keyboard Shortcuts; handled by `record_key`).
    Keybinding(crate::commands::Command),
    /// A breakpoint's condition, hit count or log message (file, line).
    Breakpoint(PathBuf, usize, super::debug::BpField),
    /// A watch expression to add (None) or change.
    Watch(Option<usize>),
    /// Emmet: Wrap with Abbreviation's abbreviation.
    EmmetWrap,
    /// An input box an extension asked for.
    Extension,
    /// The command of a custom Assistant agent.
    AgentCommand,
    /// A new name for the Assistant chat with this id.
    ChatName(String),
}

/// What to run once the commit in progress succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AfterCommit {
    Nothing,
    Push,
    Sync,
}

pub(super) struct CloneJob {
    url: String,
    dest: PathBuf,
    result: Receiver<Result<(), String>>,
}

#[derive(Default)]
pub(super) struct GitState {
    pub(super) after_commit: Option<AfterCommit>,
    /// Operations whose failures aren't reported (automatic fetches).
    quiet: Vec<Op>,
    last_fetch: Option<Instant>,
    pub(super) clone: Option<CloneJob>,
    /// The merge message was put in the commit box for the merge in progress.
    merge_prefilled: bool,
    /// Conflict blocks per document.
    conflicts: HashMap<usize, ConflictCache>,
    /// When the spinner started (it turns while a remote operation or clone runs).
    spin_epoch: Option<Instant>,
    /// Receives credential prompts from git and ssh.
    pub(super) askpass: Option<scm::askpass::Server>,
    /// The prompt shown in the quick input. Dropping it unanswered cancels it.
    credential: Option<scm::askpass::Request>,
}

impl GitState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.conflicts.clear();
    }
}

const PRESS_ENTER: &str = "(Press 'Enter' to confirm or 'Escape' to cancel)";
/// Spinner frame time.
const SPIN_FRAME: Duration = Duration::from_millis(50);

/// The check for names git would reject (`git check-ref-format` rules).
fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '/', '-'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.contains("/.")
        && name != "@"
        && !name.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
}

/// Branch names get whitespace replaced by dashes, like `git.branchWhitespaceChar`.
fn sanitize_branch(name: &str) -> String {
    name.trim().split_whitespace().collect::<Vec<_>>().join("-")
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn cmd_entry(cmd: Command) -> (PopupItem, PopupAction) {
    (PopupItem::Item { label: cmd.menu_label().to_string(), enabled: true, checked: None }, PopupAction::Run(cmd))
}

fn separator() -> (PopupItem, PopupAction) {
    (PopupItem::Separator, PopupAction::None)
}

fn submenu(label: &str, entries: Vec<(PopupItem, PopupAction)>) -> (PopupItem, PopupAction) {
    (PopupItem::Item { label: label.into(), enabled: true, checked: None }, PopupAction::Submenu(entries))
}

impl Workbench {
    // ------------------------------------------------------------------ helpers

    fn git_status(&self) -> Option<&scm::Status> {
        self.repo.as_ref().map(|r| &r.status)
    }

    pub(super) fn git_run(&mut self, op: Op) {
        if let Some(repo) = &mut self.repo {
            repo.run(op);
            if repo.busy().is_some_and(Op::is_remote) && self.git.spin_epoch.is_none() {
                self.git.spin_epoch = Some(Instant::now());
            }
        }
    }

    fn git_info(&self, title: &str) {
        self.message_dialog().set_level(rfd::MessageLevel::Info).set_title(title).show();
    }

    fn git_error(&self, error: &str) {
        let msg = first_lines(error.trim(), 12);
        self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Git").set_description(msg).show();
    }

    /// A dialog with two choices besides Cancel. Returns 0, 1, or None for Cancel.
    fn ask2(&self, title: &str, detail: &str, first: &str, second: &str) -> Option<usize> {
        let answer = self
            .message_dialog()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(title)
            .set_description(detail)
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom(first.into(), second.into(), "Cancel".into()))
            .show();
        match answer {
            rfd::MessageDialogResult::Custom(s) if s == first => Some(0),
            rfd::MessageDialogResult::Custom(s) if s == second => Some(1),
            rfd::MessageDialogResult::Yes => Some(0),
            rfd::MessageDialogResult::No => Some(1),
            _ => None,
        }
    }

    fn set_git_setting(&mut self, key: &'static str, value: bool) {
        self.update_setting(Scope::User, key, Some(Value::Bool(value)));
    }

    pub(super) fn open_input(&mut self, prompt: &str, placeholder: &str, purpose: GitInput, value: &str) {
        let input = InputBox { prompt: format!("{prompt} {PRESS_ENTER}"), placeholder: placeholder.into(), purpose, error: None, password: false };
        let mut p = Palette::with_input(input, value);
        self.validate_input(&mut p);
        self.palette = Some(p);
    }

    fn open_picker(&mut self, placeholder: &str, choices: Vec<Item>) {
        self.palette = Some(Palette::with_picker(Picker { placeholder: placeholder.into(), choices }));
    }

    fn item(label: &str, detail: &str, pick: GitPick, group: Option<&str>) -> Item {
        Item {
            label: label.into(),
            detail: detail.into(),
            matches: Vec::new(),
            shortcut: None,
            action: Action::Git(pick),
            group: group.map(str::to_string),
            kind: None,
        }
    }

    /// Pickable refs, grouped: branches, remote branches, tags.
    fn ref_items(&self, keep: impl Fn(&Ref) -> bool, pick: impl Fn(&Ref) -> GitPick) -> Vec<Item> {
        let Some(st) = self.git_status() else { return Vec::new() };
        let mut items = Vec::new();
        for (kind, group) in [(RefKind::Head, "branches"), (RefKind::Remote, "remote branches"), (RefKind::Tag, "tags")] {
            for r in st.refs.iter().filter(|r| r.kind == kind && keep(r)) {
                let detail = if r.subject.is_empty() { r.commit.clone() } else { format!("{} · {}", r.commit, r.subject) };
                items.push(Self::item(&r.name, &detail, pick(r), Some(group)));
            }
        }
        items
    }

    // ------------------------------------------------------------------ commands

    pub(super) fn git_command(&mut self, cmd: Command) {
        if cmd == Command::GitClone {
            return self.open_input("Repository URL", "Provide repository URL", GitInput::CloneUrl, "");
        }
        if matches!(
            cmd,
            Command::ConflictAcceptAllCurrent
                | Command::ConflictAcceptAllIncoming
                | Command::ConflictAcceptAllBoth
                | Command::ConflictNext
                | Command::ConflictPrevious
        ) {
            return self.conflict_command(cmd);
        }
        let Some(st) = self.git_status().cloned() else { return };
        let branch = st.branch.clone();
        match cmd {
            Command::GitCheckout => self.checkout_picker(),
            Command::GitCheckoutDetached => self.detached_picker(),
            Command::GitCreateBranch => {
                self.open_input("Please provide a new branch name", "Branch name", GitInput::NewBranch { from: None }, "")
            }
            Command::GitCreateBranchFrom => {
                let items = self.ref_items(|_| true, |r| GitPick::BranchFrom(r.name.clone()));
                self.open_picker("Select a ref to create the branch from", items);
            }
            Command::GitRenameBranch => match &branch {
                Some(b) => self.open_input("Please provide a new branch name", "Branch name", GitInput::RenameBranch, b),
                None => self.git_info("Please check out a branch to rename."),
            },
            Command::GitDeleteBranch => {
                let items = self.ref_items(|r| r.kind == RefKind::Head && !r.current, |r| GitPick::DeleteBranch(r.name.clone()));
                if items.is_empty() {
                    return self.git_info("There are no other branches to delete.");
                }
                self.open_picker("Select a branch to delete", items);
            }
            Command::GitMerge => {
                let current = branch.clone();
                let items = self.ref_items(|r| Some(&r.name) != current.as_ref(), |r| GitPick::Merge(r.name.clone()));
                self.open_picker("Select a branch or tag to merge from", items);
            }
            Command::GitRebase => {
                let current = branch.clone();
                let items = self.ref_items(|r| r.kind != RefKind::Tag && Some(&r.name) != current.as_ref(), |r| GitPick::Rebase(r.name.clone()));
                self.open_picker("Select a branch to rebase onto", items);
            }
            Command::GitAbortMerge if !st.merging => self.git_info("There is no merge in progress."),
            Command::GitAbortMerge => self.git_run(Op::AbortMerge),
            Command::GitAbortRebase | Command::GitContinueRebase if !st.rebasing => self.git_info("There is no rebase in progress."),
            Command::GitAbortRebase => self.git_run(Op::AbortRebase),
            Command::GitContinueRebase => self.git_run(Op::ContinueRebase),
            Command::GitFetch | Command::GitFetchPrune | Command::GitFetchAll => {
                if st.remotes.is_empty() {
                    return self.git_info("This repository has no remotes configured to fetch from.");
                }
                let prune = cmd == Command::GitFetchPrune || self.settings.bool("git.pruneOnFetch");
                self.git_run(Op::Fetch { remote: None, all: cmd == Command::GitFetchAll, prune });
            }
            Command::GitPull | Command::GitPullRebase => {
                if st.remotes.is_empty() {
                    return self.git_info("Your repository has no remotes configured to pull from.");
                }
                self.git_run(Op::Pull { rebase: cmd == Command::GitPullRebase });
            }
            Command::GitPush => self.push(false),
            Command::GitPushForce => self.push(true),
            Command::GitSync => self.sync(),
            Command::GitPublish => self.publish(),
            Command::GitCommitAmend => self.commit_with(true, AfterCommit::Nothing),
            Command::GitCommitPush => self.commit_with(false, AfterCommit::Push),
            Command::GitCommitSync => self.commit_with(false, AfterCommit::Sync),
            Command::GitUndoCommit if st.head.is_none() => self.git_info("Can't undo because HEAD doesn't point to any commit."),
            Command::GitUndoCommit => self.git_run(Op::UndoLastCommit),
            Command::GitUnstageAll => self.scm_action(super::scm_view::ScmAction::UnstageAll),
            Command::GitDiscardAll => self.scm_action(super::scm_view::ScmAction::DiscardAll),
            Command::GitStash | Command::GitStashUntracked | Command::GitStashStaged => {
                let staged = cmd == Command::GitStashStaged;
                if staged && st.staged.is_empty() {
                    return self.git_info("There are no staged changes to stash.");
                }
                if st.change_count() == 0 {
                    return self.git_info("There are no changes to stash.");
                }
                let untracked = cmd == Command::GitStashUntracked;
                let purpose = GitInput::StashMessage { untracked, staged };
                self.open_input("Optionally provide a stash message", "Stash message", purpose, "");
            }
            Command::GitStashPopLatest | Command::GitStashApplyLatest if st.stashes.is_empty() => {
                self.git_info("There are no stashes in the repository.")
            }
            Command::GitStashPopLatest => self.git_run(Op::StashPop(0)),
            Command::GitStashApplyLatest => self.git_run(Op::StashApply(0)),
            Command::GitStashPop | Command::GitStashApply | Command::GitStashDrop => {
                if st.stashes.is_empty() {
                    return self.git_info("There are no stashes in the repository.");
                }
                let (placeholder, pick): (&str, fn(usize) -> GitPick) = match cmd {
                    Command::GitStashPop => ("Choose a stash to pop", GitPick::StashPop),
                    Command::GitStashApply => ("Choose a stash to apply", GitPick::StashApply),
                    _ => ("Choose a stash to drop", GitPick::StashDrop),
                };
                let items = st.stashes.iter().map(|s| Self::item(&s.description, &format!("stash@{{{}}}", s.index), pick(s.index), None)).collect();
                self.open_picker(placeholder, items);
            }
            Command::GitStashDropAll => {
                let n = st.stashes.len();
                if n == 0 {
                    return self.git_info("There are no stashes in the repository.");
                }
                let title = if n == 1 {
                    "Are you sure you want to drop ALL stashes? There is 1 stash that will be subject to pruning, and MAY BE IMPOSSIBLE TO RECOVER.".to_string()
                } else {
                    format!("Are you sure you want to drop ALL stashes? There are {n} stashes that will be subject to pruning, and MAY BE IMPOSSIBLE TO RECOVER.")
                };
                if self.confirm(&title, "", "Drop All Stashes") {
                    self.git_run(Op::StashClear);
                }
            }
            Command::GitAddRemote => self.open_input("Please provide the repository URL", "Remote URL", GitInput::RemoteUrl, ""),
            Command::GitRemoveRemote => {
                if st.remotes.is_empty() {
                    return self.git_info("Your repository has no remotes.");
                }
                let items = st.remotes.iter().map(|r| Self::item(&r.name, &r.fetch_url, GitPick::RemoveRemote(r.name.clone()), None)).collect();
                self.open_picker("Pick a remote to remove", items);
            }
            Command::GitCreateTag => self.open_input("Please provide a tag name", "Tag name", GitInput::TagName, ""),
            Command::GitDeleteTag => {
                let items = self.ref_items(|r| r.kind == RefKind::Tag, |r| GitPick::DeleteTag(r.name.clone()));
                if items.is_empty() {
                    return self.git_info("This repository has no tags.");
                }
                self.open_picker("Select a tag to delete", items);
            }
            _ => {}
        }
    }

    /// The status bar's sync item: publish a branch without an upstream, otherwise sync.
    pub(super) fn status_sync_clicked(&mut self) {
        if let Some(st) = self.git_status().filter(|s| s.upstream.is_none()).cloned() {
            // Publishing makes the branch public: ask first (with several remotes, the picker asks).
            if let ([remote], Some(branch)) = (st.remotes.as_slice(), &st.branch) {
                let title = format!("Publish the branch '{branch}' to {}?", remote.name);
                let detail = format!("It will be pushed to {} and tracked from there.", remote.push_url);
                if !self.confirm(&title, &detail, "Publish Branch") {
                    return;
                }
            }
            self.publish();
        } else {
            self.sync();
        }
    }

    /// The branch picker behind "Checkout to..." and the status bar branch item.
    pub(super) fn checkout_picker(&mut self) {
        if self.repo.is_none() {
            return;
        }
        let mut items = vec![
            Self::item("+ Create new branch...", "", GitPick::NewBranch, None),
            Self::item("+ Create new branch from...", "", GitPick::NewBranchFrom, None),
            Self::item("Checkout detached...", "", GitPick::DetachedPicker, None),
        ];
        items.extend(self.ref_items(|r| !r.current, |r| GitPick::Checkout(r.name.clone())));
        self.open_picker("Select a branch or tag to checkout", items);
    }

    fn detached_picker(&mut self) {
        let items = self.ref_items(|_| true, |r| GitPick::CheckoutDetached(r.name.clone()));
        self.open_picker("Select a branch or tag to checkout in detached mode", items);
    }

    pub(super) fn git_pick(&mut self, pick: GitPick) {
        match pick {
            GitPick::Checkout(target) => self.git_run(Op::Checkout { target, detached: false }),
            GitPick::CheckoutDetached(target) => self.git_run(Op::Checkout { target, detached: true }),
            GitPick::NewBranch => self.git_command(Command::GitCreateBranch),
            GitPick::NewBranchFrom => self.git_command(Command::GitCreateBranchFrom),
            GitPick::DetachedPicker => self.detached_picker(),
            GitPick::BranchFrom(from) => {
                self.open_input("Please provide a new branch name", "Branch name", GitInput::NewBranch { from: Some(from) }, "")
            }
            GitPick::DeleteBranch(name) => self.git_run(Op::DeleteBranch { name, force: false }),
            GitPick::Merge(target) => self.git_run(Op::Merge(target)),
            GitPick::Rebase(target) => self.git_run(Op::Rebase(target)),
            GitPick::Publish(remote) => self.git_run(Op::Push { publish: Some(remote), force: false }),
            GitPick::StashPop(i) => self.git_run(Op::StashPop(i)),
            GitPick::StashApply(i) => self.git_run(Op::StashApply(i)),
            GitPick::StashDrop(i) => {
                let desc = self.git_status().and_then(|s| s.stashes.iter().find(|st| st.index == i)).map(|s| s.description.clone());
                let title = format!("Are you sure you want to drop the stash: {}?", desc.unwrap_or_default());
                if self.confirm(&title, "", "Drop") {
                    self.git_run(Op::StashDrop(i));
                }
            }
            GitPick::RemoveRemote(name) => self.git_run(Op::RemoveRemote(name)),
            GitPick::DeleteTag(name) => self.git_run(Op::DeleteTag(name)),
        }
    }

    /// Checks the input box's value as it's typed (shows the problem under the input).
    pub(super) fn validate_input(&self, p: &mut Palette) {
        let Some(b) = &mut p.input_box else { return };
        let value = p.input.trim();
        let st = self.git_status();
        let has_ref = |kind: RefKind, name: &str| st.is_some_and(|s| s.refs.iter().any(|r| r.kind == kind && r.name == name));
        b.error = match &b.purpose {
            GitInput::NewBranch { .. } | GitInput::RenameBranch if !value.is_empty() => {
                let name = sanitize_branch(value);
                let unchanged = matches!(b.purpose, GitInput::RenameBranch) && st.and_then(|s| s.branch.as_deref()) == Some(name.as_str());
                if !valid_ref_name(&name) {
                    Some("Please provide a valid branch name".into())
                } else if has_ref(RefKind::Head, &name) && !unchanged {
                    Some(format!("A branch named '{name}' already exists"))
                } else {
                    None
                }
            }
            GitInput::RemoteName { .. } if !value.is_empty() => {
                if !valid_ref_name(value) {
                    Some("Please provide a valid remote name".into())
                } else if st.is_some_and(|s| s.remotes.iter().any(|r| r.name == value)) {
                    Some(format!("Remote '{value}' already exists."))
                } else {
                    None
                }
            }
            GitInput::TagName if !value.is_empty() => {
                if !valid_ref_name(value) {
                    Some("Please provide a valid tag name".into())
                } else if has_ref(RefKind::Tag, value) {
                    Some(format!("Tag '{value}' already exists"))
                } else {
                    None
                }
            }
            _ => None,
        };
    }

    /// Enter in an input box. Returns false to keep the box open (the value isn't valid).
    pub(super) fn git_input(&mut self, purpose: GitInput, value: String) -> bool {
        // Passwords are sent exactly as typed.
        let value = if matches!(purpose, GitInput::Credential | GitInput::Extension) { value } else { value.trim().to_string() };
        let optional = matches!(purpose, GitInput::StashMessage { .. } | GitInput::TagMessage { .. } | GitInput::Credential | GitInput::Breakpoint(..) | GitInput::Watch(Some(_)) | GitInput::Extension);
        if value.is_empty() && !optional {
            return false;
        }
        match purpose {
            GitInput::Keybinding(_) => {} // recorded key by key in `record_key`
            GitInput::Breakpoint(path, line, field) => self.set_breakpoint_field(path, line, field, value),
            GitInput::Watch(index) => self.set_watch(index, value),
            GitInput::EmmetWrap => self.emmet_wrap(value),
            GitInput::AgentCommand => self.set_custom_agent(value),
            GitInput::ChatName(id) => self.chat_named(&id, value),
            GitInput::Extension => self.ext_answer(serde_json::json!(value)),
            GitInput::NewBranch { from } => self.git_run(Op::CreateBranch { name: sanitize_branch(&value), from }),
            GitInput::RenameBranch => {
                let name = sanitize_branch(&value);
                if self.git_status().and_then(|s| s.branch.as_deref()) != Some(name.as_str()) {
                    self.git_run(Op::RenameBranch(name));
                }
            }
            GitInput::StashMessage { untracked, staged } => self.git_run(Op::Stash { message: value, untracked, staged }),
            GitInput::RemoteUrl => {
                let first = self.git_status().is_some_and(|s| s.remotes.is_empty());
                let purpose = GitInput::RemoteName { url: value };
                self.open_input("Please provide a remote name", "Remote name", purpose, if first { "origin" } else { "" });
            }
            GitInput::RemoteName { url } => self.git_run(Op::AddRemote { name: value, url }),
            GitInput::TagName => {
                let purpose = GitInput::TagMessage { name: value };
                self.open_input("Please provide a message to annotate the tag", "Message", purpose, "");
            }
            GitInput::TagMessage { name } => self.git_run(Op::CreateTag { name, message: value }),
            GitInput::CloneUrl => self.start_clone(value),
            GitInput::Credential => {
                if let Some(request) = self.git.credential.take() {
                    request.answer(Some(&value));
                }
            }
        }
        true
    }

    // ------------------------------------------------------------------ push, pull, sync

    /// Asks where to publish the current branch (or says there is nowhere to).
    fn publish(&mut self) {
        let Some(st) = self.git_status().cloned() else { return };
        let Some(branch) = st.branch else {
            return self.git_info("Please check out a branch to publish.");
        };
        match st.remotes.len() {
            0 => {
                if self.confirm("Your repository has no remotes configured to publish to.", "", "Add Remote") {
                    self.git_command(Command::GitAddRemote);
                }
            }
            1 => self.git_run(Op::Push { publish: Some(st.remotes[0].name.clone()), force: false }),
            _ => {
                let items = st.remotes.iter().map(|r| Self::item(&r.name, &r.push_url, GitPick::Publish(r.name.clone()), None)).collect();
                self.open_picker(&format!("Pick a remote to publish the branch '{branch}' to:"), items);
            }
        }
    }

    /// Offers to publish a branch that has no upstream. Returns true if it had one.
    fn ensure_upstream(&mut self) -> bool {
        let Some(st) = self.git_status().cloned() else { return false };
        if st.upstream.is_some() {
            return true;
        }
        match st.branch {
            None => self.git_info("Please check out a branch to push to a remote."),
            Some(_) if st.remotes.is_empty() => {
                if self.confirm("Your repository has no remotes configured to push to.", "", "Add Remote") {
                    self.git_command(Command::GitAddRemote);
                }
            }
            Some(b) => {
                let title = format!("The branch '{b}' has no remote branch. Would you like to publish this branch?");
                if self.confirm(&title, "", "OK") {
                    self.publish();
                }
            }
        }
        false
    }

    fn push(&mut self, force: bool) {
        if !self.ensure_upstream() {
            return;
        }
        if force && self.settings.bool("git.confirmForcePush") {
            let detail = "You are about to force push your changes, this can be destructive and could inadvertently overwrite changes made by others.\n\nAre you sure to continue?";
            match self.ask2("Force Push", detail, "OK", "Always") {
                Some(1) => self.set_git_setting("git.confirmForcePush", false),
                Some(_) => {}
                None => return,
            }
        }
        self.git_run(Op::Push { publish: None, force });
    }

    fn sync(&mut self) {
        if !self.ensure_upstream() {
            return;
        }
        let upstream = self.git_status().and_then(|s| s.upstream.clone()).unwrap_or_default();
        if self.settings.bool("git.confirmSync") {
            let title = format!("This action will pull and push commits from and to '{upstream}'.");
            match self.ask2(&title, "", "OK", "OK, Don't Show Again") {
                Some(1) => self.set_git_setting("git.confirmSync", false),
                Some(_) => {}
                None => return,
            }
        }
        let rebase = self.settings.bool("git.rebaseWhenSync");
        self.git_run(Op::Sync { rebase });
    }

    /// The commit button's secondary actions (the chevron next to it).
    pub(super) fn commit_menu_entries(&self) -> Vec<(PopupItem, PopupAction)> {
        [Command::GitCommit, Command::GitCommitAmend, Command::GitCommitPush, Command::GitCommitSync].into_iter().map(cmd_entry).collect()
    }

    /// The Source Control view's "..." menu.
    pub(super) fn more_actions_entries(&self) -> Vec<(PopupItem, PopupAction)> {
        use Command::*;
        let list = |cmds: &[Command]| -> Vec<(PopupItem, PopupAction)> {
            cmds.iter().map(|&c| if c == Command::CommandPalette { separator() } else { cmd_entry(c) }).collect()
        };
        // `CommandPalette` marks a separator in these lists.
        const SEP: Command = CommandPalette;
        let rebasing = self.git_status().is_some_and(|s| s.rebasing);
        let merging = self.git_status().is_some_and(|s| s.merging);
        let mut commit = vec![GitCommit, GitCommitAmend, GitCommitPush, GitCommitSync, SEP, GitUndoCommit];
        if merging {
            commit.push(GitAbortMerge);
        }
        if rebasing {
            commit.extend([GitContinueRebase, GitAbortRebase]);
        }
        let mut entries = list(&[GitPull, GitPush, GitClone, GitCheckout, GitFetch, SEP]);
        entries.push(submenu("Commit", list(&commit)));
        entries.push(submenu("Changes", list(&[GitStageAll, GitUnstageAll, GitDiscardAll])));
        entries.push(submenu(
            "Pull, Push",
            list(&[GitSync, SEP, GitPull, GitPullRebase, SEP, GitPush, GitPushForce, SEP, GitFetch, GitFetchPrune, GitFetchAll]),
        ));
        entries.push(submenu(
            "Branch",
            list(&[GitMerge, GitRebase, SEP, GitCreateBranch, GitCreateBranchFrom, SEP, GitRenameBranch, GitDeleteBranch, SEP, GitPublish]),
        ));
        entries.push(submenu("Remote", list(&[GitAddRemote, GitRemoveRemote])));
        entries.push(submenu(
            "Stash",
            list(&[
                GitStash,
                GitStashUntracked,
                GitStashStaged,
                SEP,
                GitStashApplyLatest,
                GitStashApply,
                SEP,
                GitStashPopLatest,
                GitStashPop,
                SEP,
                GitStashDrop,
                GitStashDropAll,
            ]),
        ));
        entries.push(submenu("Tags", list(&[GitCreateTag, GitDeleteTag])));
        entries
    }

    // ------------------------------------------------------------------ worker events

    /// Reacts to a finished operation (the status refresh follows separately).
    pub(super) fn git_done(&mut self, op: Op, output: String) {
        if let Some(i) = self.git.quiet.iter().position(|q| *q == op) {
            self.git.quiet.remove(i);
        }
        match op {
            Op::Commit { .. } => {
                self.scm.message.set_text("");
                match self.git.after_commit.take() {
                    Some(AfterCommit::Push) => self.push(false),
                    Some(AfterCommit::Sync) => self.sync(),
                    _ => {}
                }
            }
            Op::UndoLastCommit => {
                if self.scm.message.text.trim().is_empty() {
                    self.scm.message.set_text(&output);
                }
            }
            Op::Fetch { .. } => self.git.last_fetch = Some(Instant::now()),
            _ => {}
        }
        if !matches!(op, Op::Stage(_) | Op::Unstage(_) | Op::Commit { .. } | Op::Fetch { .. } | Op::Push { .. }) {
            // Checkouts, merges, pulls and stashes change files on disk.
            if let Some(tree) = &mut self.tree {
                tree.refresh();
            }
            self.palette_files = None;
        }
    }

    /// Reports a failed operation, offering a way out where there is one.
    pub(super) fn git_failed(&mut self, op: Op, error: String) {
        if let Some(i) = self.git.quiet.iter().position(|q| *q == op) {
            self.git.quiet.remove(i);
            return;
        }
        if matches!(op, Op::Commit { .. }) {
            self.git.after_commit = None;
        }
        let has = |s: &str| error.contains(s);
        match &op {
            Op::Checkout { target, detached } if has("would be overwritten by checkout") => {
                let untracked = has("untracked working tree files");
                let title = "Your local changes would be overwritten by checkout.";
                if self.confirm(title, &first_lines(&error, 8), "Stash & Checkout") {
                    self.git_run(Op::Stash { message: String::new(), untracked, staged: false });
                    self.git_run(Op::Checkout { target: target.clone(), detached: *detached });
                    self.git_run(Op::StashPop(0));
                }
            }
            Op::DeleteBranch { name, force: false } if has("not fully merged") => {
                let title = format!("The branch '{name}' is not fully merged. Delete anyway?");
                if self.confirm(&title, "", "Delete Branch") {
                    self.git_run(Op::DeleteBranch { name: name.clone(), force: true });
                }
            }
            Op::Push { .. } | Op::Sync { .. } if has("[rejected]") || has("non-fast-forward") || has("fetch first") => {
                let title = "Can't push refs to remote. Try running 'Pull' first to integrate your changes.";
                if self.confirm(title, "", "Pull") {
                    self.git_run(Op::Pull { rebase: false });
                }
            }
            _ if has("CONFLICT") || has("Merge conflict") => {
                self.message_dialog()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("There are merge conflicts. Resolve them before committing.")
                    .show();
                self.view = View::Scm;
                self.sidebar_visible = true;
            }
            _ => self.git_error(&error),
        }
        if let Some(tree) = &mut self.tree {
            tree.refresh();
        }
    }

    /// Per-frame git upkeep: credential prompts, the merge message, automatic fetches and
    /// clone jobs.
    pub(super) fn git_tick(&mut self) {
        self.credential_tick();
        let (merging, message) = match self.git_status() {
            Some(st) => (st.merging, st.merge_message.clone()),
            None => (false, None),
        };
        if merging && !self.git.merge_prefilled {
            self.git.merge_prefilled = true;
            if let Some(m) = message.filter(|_| self.scm.message.text.trim().is_empty()) {
                self.scm.message.set_text(&m);
            }
        } else if !merging {
            self.git.merge_prefilled = false;
        }

        if let (Some(period), Some(repo)) = (crate::config::get().git_autofetch_secs, &self.repo) {
            let due = self.git.last_fetch.is_none_or(|t| t.elapsed() >= Duration::from_secs(period));
            if due && repo.busy().is_none() && !repo.status.remotes.is_empty() {
                let op = Op::Fetch { remote: None, all: false, prune: self.settings.bool("git.pruneOnFetch") };
                self.git.quiet.push(op.clone());
                self.git.last_fetch = Some(Instant::now());
                // Automatic fetches never ask for credentials.
                if let Some(repo) = &mut self.repo {
                    repo.run_quiet(op);
                }
            }
        }

        let finished = self.git.clone.as_ref().and_then(|j| j.result.try_recv().ok());
        if let Some(result) = finished {
            let job = self.git.clone.take().unwrap();
            match result {
                Ok(()) => {
                    let open = self.confirm("Would you like to open the cloned repository?", &job.dest.display().to_string(), "Open");
                    if open {
                        self.open_folder(&job.dest);
                    }
                }
                Err(e) => self.git_error(&format!("Failed to clone '{}':\n{e}", job.url)),
            }
        }
        let spinning = self.git_spinning();
        if !spinning {
            self.git.spin_epoch = None;
        } else if self.git.spin_epoch.is_none() {
            self.git.spin_epoch = Some(Instant::now());
        }
    }

    /// Whether a slow git operation (remote access, clone) is running.
    pub(super) fn git_spinning(&self) -> bool {
        self.git.clone.is_some() || self.repo.as_ref().and_then(|r| r.busy()).is_some_and(Op::is_remote)
    }

    /// The spinner's rotation step (for `Canvas::icon_turned`).
    pub(super) fn git_spin_turn(&self) -> u32 {
        self.git.spin_epoch.map_or(0, |t| (t.elapsed().as_millis() / SPIN_FRAME.as_millis()) as u32)
    }

    /// When git needs the next frame: spinner animation or the next automatic fetch.
    pub(super) fn git_deadline(&self) -> Option<Instant> {
        let spin = self.git.spin_epoch.filter(|_| self.git_spinning()).map(|_| Instant::now() + SPIN_FRAME);
        let fetch = crate::config::get().git_autofetch_secs.filter(|_| self.repo.is_some()).map(|p| {
            self.git.last_fetch.map_or_else(Instant::now, |t| t + Duration::from_secs(p))
        });
        spin.into_iter().chain(fetch).min()
    }

    /// Status bar text while cloning.
    pub(super) fn git_progress_text(&self) -> Option<String> {
        self.git.clone.as_ref().map(|j| format!("Cloning git repository '{}'...", j.url))
    }

    /// Resets per-folder git state (a folder was opened or closed).
    pub(super) fn reset_git_state(&mut self) {
        let (clone, askpass) = (self.git.clone.take(), self.git.askpass.take());
        self.git = GitState { clone, askpass, ..Default::default() };
        self.git.last_fetch = Some(Instant::now());
    }

    // ------------------------------------------------------------------ credentials

    /// Starts answering git's and ssh's credential prompts (once, at startup).
    pub(super) fn start_askpass(&mut self) {
        let Ok(exe) = std::env::current_exe() else { return };
        match scm::askpass::Server::start(&exe, self.waker.clone()) {
            Ok(server) => self.git.askpass = Some(server),
            Err(e) => eprintln!("askpass: {e}"),
        }
    }

    /// Shows the next credential prompt, and cancels one whose input box was closed.
    fn credential_tick(&mut self) {
        let showing = self.palette.as_ref().and_then(|p| p.input_box.as_ref()).is_some_and(|b| matches!(b.purpose, GitInput::Credential));
        if self.git.credential.is_some() && !showing {
            self.git.credential = None; // dropped unanswered: git or ssh gives up
        }
        if self.git.credential.is_some() {
            return;
        }
        let Some(request) = self.git.askpass.as_ref().and_then(|s| s.poll()) else { return };
        // ssh's host key question spans several lines; the last one is the question.
        let lines: Vec<&str> = request.prompt.lines().filter(|l| !l.trim().is_empty()).collect();
        let question = lines.last().copied().unwrap_or_default().trim().trim_end_matches(':').trim().to_string();
        let prompt = match lines.split_last() {
            Some((_, [])) | None => format!("Git: {question} {PRESS_ENTER}"),
            Some((last, rest)) => format!("{}\n{} {PRESS_ENTER}", rest.join("\n"), last.trim()),
        };
        let input = InputBox { prompt, placeholder: question, purpose: GitInput::Credential, error: None, password: request.is_secret() };
        self.git.credential = Some(request);
        self.palette = Some(Palette::with_input(input, ""));
    }

    // ------------------------------------------------------------------ clone

    fn start_clone(&mut self, url: String) {
        if self.git.clone.is_some() {
            return self.git_info("A clone is already in progress.");
        }
        let Some(parent) = self.file_dialog().set_title("Choose a folder to clone the repository into").pick_folder() else { return };
        let dest = parent.join(scm::clone_dir_name(&url));
        let (tx, rx) = mpsc::channel();
        let waker = self.waker.clone();
        let (u, d) = (url.clone(), dest.clone());
        let spawned = std::thread::Builder::new().name("git-clone".into()).spawn(move || {
            let _ = tx.send(scm::clone(&u, &d));
            waker();
        });
        if spawned.is_ok() {
            self.git.clone = Some(CloneJob { url, dest, result: rx });
            self.git.spin_epoch = Some(Instant::now());
        }
    }

    // ------------------------------------------------------------------ merge conflicts

    /// Conflict blocks in a document (cached per edit).
    pub(super) fn doc_conflicts(&mut self, doc_id: usize) -> Vec<crate::conflicts::Conflict> {
        let Some(doc) = self.docs.get(doc_id).and_then(Option::as_ref) else { return Vec::new() };
        self.git.conflicts.entry(doc_id).or_default().update(doc).to_vec()
    }

    /// A click on "Accept Current Change" (etc.) above a conflict.
    pub(super) fn click_conflict_action(&mut self, g: usize, i: usize) {
        self.active_group = g;
        self.focus = Focus::Editor;
        let Some((ed, doc)) = self.active_mut() else { return };
        let Some(&(_, conflict, how)) = ed.conflict_actions.get(i) else { return };
        ed.resolve_conflict(doc, conflict, how);
    }

    fn conflict_command(&mut self, cmd: Command) {
        self.focus = Focus::Editor;
        let Some((ed, doc)) = self.active_mut() else { return };
        let found = match cmd {
            Command::ConflictNext => ed.go_to_conflict(doc, true),
            Command::ConflictPrevious => ed.go_to_conflict(doc, false),
            Command::ConflictAcceptAllCurrent => ed.resolve_all_conflicts(doc, Resolution::Current) > 0,
            Command::ConflictAcceptAllIncoming => ed.resolve_all_conflicts(doc, Resolution::Incoming) > 0,
            _ => ed.resolve_all_conflicts(doc, Resolution::Both) > 0,
        };
        if !found {
            self.set_status_message("No merge conflicts found in this file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        assert_eq!(sanitize_branch("  my new  branch "), "my-new-branch");
        assert!(valid_ref_name("feature/login"));
        assert!(!valid_ref_name("feature..x"));
        assert!(!valid_ref_name("-x"));
        assert!(!valid_ref_name("x.lock"));
        assert!(!valid_ref_name("a:b"));
        assert!(!valid_ref_name("a/"));
    }
}
