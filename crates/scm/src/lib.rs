//! Source control: a git repository with a background worker, so git never blocks the UI.

pub mod askpass;
mod diff;
mod git;
pub mod graph;

use std::path::{Path, PathBuf};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

pub use diff::{apply_changes, hunks, line_changes, side_by_side, DiffRow, LineChange};
pub use git::{available as git_available, clone, clone_dir_name, commit_refs, git_dir, repo_root, show, Change, LogEntry, FileStatus, Ref, RefKind, Remote, Stash, Status};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A git operation that changes the repository. Runs on the worker; each one ends with
/// `Event::Done` or `Event::Failed`, followed by a fresh status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    Discard(Vec<PathBuf>),
    Commit { message: String, all: bool, amend: bool },
    UndoLastCommit,
    /// Checks out a branch, tag or commit. A remote branch ("origin/x") gets a local
    /// tracking branch.
    Checkout { target: String, detached: bool },
    CreateBranch { name: String, from: Option<String> },
    RenameBranch(String),
    DeleteBranch { name: String, force: bool },
    Merge(String),
    AbortMerge,
    Rebase(String),
    AbortRebase,
    ContinueRebase,
    /// `remote: None` fetches the default remote; `all` fetches every remote.
    Fetch { remote: Option<String>, all: bool, prune: bool },
    Pull { rebase: bool },
    /// Pushes to the upstream, or with `publish`, to that remote with `--set-upstream`.
    Push { publish: Option<String>, force: bool },
    /// Pull, then push.
    Sync { rebase: bool },
    Stash { message: String, untracked: bool, staged: bool },
    StashPop(usize),
    StashApply(usize),
    StashDrop(usize),
    StashClear,
    AddRemote { name: String, url: String },
    RemoveRemote(String),
    CreateTag { name: String, message: String },
    DeleteTag(String),
    /// Sets a file's staged contents (Stage / Unstage Selected Ranges).
    StageContents { path: PathBuf, contents: String },
}

impl Op {
    /// What's running, for progress indicators ("Git: Pushing...").
    pub fn progress(&self) -> &'static str {
        match self {
            Op::Stage(_) => "Staging",
            Op::Unstage(_) => "Unstaging",
            Op::Discard(_) => "Discarding",
            Op::Commit { .. } => "Committing",
            Op::UndoLastCommit => "Undoing commit",
            Op::Checkout { .. } => "Checking out",
            Op::CreateBranch { .. } => "Creating branch",
            Op::RenameBranch(_) => "Renaming branch",
            Op::DeleteBranch { .. } => "Deleting branch",
            Op::Merge(_) => "Merging",
            Op::AbortMerge => "Aborting merge",
            Op::Rebase(_) | Op::ContinueRebase => "Rebasing",
            Op::AbortRebase => "Aborting rebase",
            Op::Fetch { .. } => "Fetching",
            Op::Pull { .. } => "Pulling",
            Op::Push { .. } => "Pushing",
            Op::Sync { .. } => "Synchronizing",
            Op::Stash { .. } => "Stashing",
            Op::StashPop(_) | Op::StashApply(_) => "Applying stash",
            Op::StashDrop(_) | Op::StashClear => "Dropping stash",
            Op::AddRemote { .. } | Op::RemoveRemote(_) => "Updating remotes",
            Op::CreateTag { .. } | Op::DeleteTag(_) => "Updating tags",
            Op::StageContents { .. } => "Staging",
        }
    }

    /// Whether the operation talks to a remote (slow; shown with a spinner).
    pub fn is_remote(&self) -> bool {
        matches!(self, Op::Fetch { .. } | Op::Pull { .. } | Op::Push { .. } | Op::Sync { .. })
    }
}

pub enum Job {
    Refresh,
    Run(Op),
    /// Run without credential prompts (automatic fetches), failing instead.
    RunQuiet(Op),
    /// Load a file's HEAD contents (for gutter diffs).
    HeadContents(PathBuf),
    /// Load the commits that changed a file (the Timeline).
    FileLog(PathBuf),
    /// Load the history of HEAD and its upstream (the Source Control Graph).
    Graph,
    /// Load the files a commit changed.
    CommitFiles(String),
}

pub enum Event {
    Status(Status),
    /// An operation succeeded. `output` is the text it produced that the UI may want (the
    /// message of an undone commit).
    Done { op: Op, output: String },
    Failed { op: Op, error: String },
    /// A refresh failed.
    Error(String),
    HeadContents(PathBuf, Option<String>),
    FileLog(PathBuf, Vec<LogEntry>),
    Graph(Vec<graph::GraphCommit>),
    /// A commit's changed files: (status letter, path).
    CommitFiles(String, Vec<(char, PathBuf)>),
}

/// Runs one operation. `initial`: the repository has no commits yet.
fn run_op(root: &Path, op: &Op, initial: bool) -> Result<String, String> {
    let exec = |args: &[&str]| git::exec(root, args);
    match op {
        Op::Stage(paths) => git::stage(root, paths).map(|_| String::new()),
        Op::StageContents { path, contents } => git::stage_contents(root, path, contents).map(|_| String::new()),
        Op::Unstage(paths) => git::unstage(root, paths, initial).map(|_| String::new()),
        Op::Discard(paths) => git::discard(root, paths).map(|_| String::new()),
        Op::Commit { message, all, amend } => git::commit(root, message, *all, *amend).map(|_| String::new()),
        Op::UndoLastCommit => {
            let message = git::head_message(root).unwrap_or_default();
            // The first commit has no parent to reset to: delete the branch ref instead.
            let has_parent = exec(&["rev-parse", "--verify", "-q", "HEAD~1"]).is_ok();
            if has_parent {
                exec(&["reset", "--soft", "HEAD~1"])?;
            } else {
                exec(&["update-ref", "-d", "HEAD"])?;
            }
            Ok(message)
        }
        Op::Checkout { target, detached: true } => exec(&["checkout", "-q", "--detach", target]),
        Op::Checkout { target, detached: false } => {
            let local = exec(&["rev-parse", "--verify", "-q", &format!("refs/heads/{target}")]).is_ok();
            let remote = exec(&["rev-parse", "--verify", "-q", &format!("refs/remotes/{target}")]).is_ok();
            if !local && remote {
                // "origin/topic" → a local "topic" tracking it (or just switch if it exists).
                let name = target.split_once('/').map_or(target.as_str(), |(_, n)| n);
                if exec(&["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")]).is_ok() {
                    return exec(&["checkout", "-q", name]);
                }
                return exec(&["checkout", "-q", "--track", "-b", name, target]);
            }
            exec(&["checkout", "-q", target])
        }
        Op::CreateBranch { name, from } => {
            let mut args = vec!["checkout", "-q", "-b", name.as_str()];
            args.extend(from.as_deref());
            exec(&args)
        }
        Op::RenameBranch(name) => exec(&["branch", "-m", name]),
        Op::DeleteBranch { name, force } => exec(&["branch", if *force { "-D" } else { "-d" }, name]),
        Op::Merge(target) => exec(&["merge", "--no-edit", target]),
        Op::AbortMerge => exec(&["merge", "--abort"]),
        Op::Rebase(target) => exec(&["rebase", target]),
        Op::AbortRebase => exec(&["rebase", "--abort"]),
        Op::ContinueRebase => exec(&["-c", "core.editor=true", "rebase", "--continue"]),
        Op::Fetch { remote, all, prune } => {
            let mut args = vec!["fetch"];
            if *all {
                args.push("--all");
            }
            if *prune {
                args.push("--prune");
            }
            args.extend(remote.as_deref());
            exec(&args)
        }
        Op::Pull { rebase } => exec(&["pull", "--no-edit", if *rebase { "--rebase" } else { "--no-rebase" }]),
        Op::Push { publish: Some(remote), force } => {
            let mut args = vec!["push", "-u", remote.as_str(), "HEAD"];
            if *force {
                args.push("--force-with-lease");
            }
            exec(&args)
        }
        Op::Push { publish: None, force } => exec(if *force { &["push", "--force-with-lease"] } else { &["push"] }),
        Op::Sync { rebase } => {
            exec(&["pull", "--no-edit", if *rebase { "--rebase" } else { "--no-rebase" }])?;
            exec(&["push"])
        }
        Op::Stash { message, untracked, staged } => {
            let mut args = vec!["stash", "push"];
            if *untracked {
                args.push("--include-untracked");
            }
            if *staged {
                args.push("--staged");
            }
            if !message.is_empty() {
                args.extend(["-m", message.as_str()]);
            }
            exec(&args)
        }
        Op::StashPop(i) => exec(&["stash", "pop", "--index", &format!("stash@{{{i}}}")]),
        Op::StashApply(i) => exec(&["stash", "apply", "--index", &format!("stash@{{{i}}}")]),
        Op::StashDrop(i) => exec(&["stash", "drop", &format!("stash@{{{i}}}")]),
        Op::StashClear => exec(&["stash", "clear"]),
        Op::AddRemote { name, url } => exec(&["remote", "add", name, url]),
        Op::RemoveRemote(name) => exec(&["remote", "remove", name]),
        Op::CreateTag { name, message } if message.is_empty() => exec(&["tag", name]),
        Op::CreateTag { name, message } => exec(&["tag", "-a", name, "-m", message]),
        Op::DeleteTag(name) => exec(&["tag", "-d", name]),
    }
}

pub struct Repo {
    pub root: PathBuf,
    pub status: Status,
    tx: Sender<Job>,
    rx: Receiver<Event>,
    /// Operations sent and not finished yet, oldest first (the worker runs them in order).
    running: VecDeque<Op>,
}

impl Repo {
    /// Opens the repository containing `dir` (if any) and starts its worker.
    pub fn open(dir: &Path, waker: Waker) -> Option<Self> {
        let root = git::repo_root(dir)?;
        let git_dir = git::git_dir(&root).unwrap_or_else(|| root.join(".git"));
        let (tx, jobs) = mpsc::channel::<Job>();
        let (events, rx) = mpsc::channel::<Event>();
        let worker_root = root.clone();
        thread::Builder::new()
            .name("git".into())
            .spawn(move || {
                let root = worker_root;
                let mut initial = false;
                while let Ok(first) = jobs.recv() {
                    // Collapse queued refreshes into one.
                    let mut batch = vec![first];
                    batch.extend(jobs.try_iter());
                    let mut refresh = false;
                    for job in batch {
                        match job {
                            Job::Refresh => {}
                            Job::Run(op) => {
                                let event = match run_op(&root, &op, initial) {
                                    Ok(output) => Event::Done { op, output },
                                    Err(error) => Event::Failed { op, error },
                                };
                                let _ = events.send(event);
                            }
                            Job::RunQuiet(op) => {
                                git::NO_PROMPTS.with(|n| n.set(true));
                                let result = run_op(&root, &op, initial);
                                git::NO_PROMPTS.with(|n| n.set(false));
                                let _ = events.send(match result {
                                    Ok(output) => Event::Done { op, output },
                                    Err(error) => Event::Failed { op, error },
                                });
                            }
                            Job::HeadContents(path) => {
                                let text = git::head_contents(&root, &path);
                                let _ = events.send(Event::HeadContents(path, text));
                                continue;
                            }
                            Job::FileLog(path) => {
                                let log = git::file_log(&root, &path, 200);
                                let _ = events.send(Event::FileLog(path, log));
                                continue;
                            }
                            Job::Graph => {
                                let _ = events.send(Event::Graph(git::graph_log(&root, 300)));
                                continue;
                            }
                            Job::CommitFiles(hash) => {
                                let files = git::commit_files(&root, &hash);
                                let _ = events.send(Event::CommitFiles(hash, files));
                                continue;
                            }
                        }
                        refresh = true;
                    }
                    if refresh {
                        match git::status(&root, &git_dir) {
                            Ok(st) => {
                                initial = st.initial;
                                let _ = events.send(Event::Status(st));
                            }
                            Err(e) => {
                                let _ = events.send(Event::Error(e));
                            }
                        }
                    }
                    waker();
                }
            })
            .ok()?;
        let _ = tx.send(Job::Refresh);
        Some(Self { root, status: Status::default(), tx, rx, running: VecDeque::new() })
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }

    /// Starts an operation on the worker.
    pub fn run(&mut self, op: Op) {
        self.running.push_back(op.clone());
        self.send(Job::Run(op));
    }

    /// Starts an operation that must not ask for credentials (it fails instead).
    pub fn run_quiet(&mut self, op: Op) {
        self.running.push_back(op.clone());
        self.send(Job::RunQuiet(op));
    }

    /// The operation running now, if any.
    pub fn busy(&self) -> Option<&Op> {
        self.running.front()
    }

    /// Events since the last call. Status updates are applied to `self.status` as well.
    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<Event> = self.rx.try_iter().collect();
        for e in &events {
            match e {
                Event::Status(st) => self.status = st.clone(),
                Event::Done { .. } | Event::Failed { .. } => {
                    self.running.pop_front();
                }
                _ => {}
            }
        }
        events
    }

    /// The status of a single file, preferring the working-tree change.
    pub fn file_status(&self, path: &Path) -> Option<FileStatus> {
        let st = &self.status;
        st.conflicts
            .iter()
            .chain(&st.unstaged)
            .chain(&st.staged)
            .find(|c| c.path == path)
            .map(|c| c.status)
    }

    /// The current branch's entry in the ref list.
    pub fn head_ref(&self) -> Option<&Ref> {
        self.status.refs.iter().find(|r| r.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?} failed");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbvane-scm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn init(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "test@example.com"]);
        run(dir, &["config", "user.name", "Test"]);
    }

    #[test]
    fn stages_part_of_a_file() {
        let dir = temp_dir("partial");
        init(&dir);
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        run(&dir, &["add", "a.txt"]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        // Two changes in the working file; stage only the first.
        let working = "ONE\ntwo\nthree\nfour\n";
        std::fs::write(&file, working).unwrap();
        let index = show(&dir, "", &file).unwrap();
        let staged = apply_changes(&index, working, false, |r| Some(r).filter(|r| r.start == 0));
        git::stage_contents(&dir, &file, &staged).unwrap();
        assert_eq!(show(&dir, "", &file).unwrap(), "ONE\ntwo\nthree\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), working);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Waits for the next status, failing on errors (unless `failures` collects them).
    fn wait_status(repo: &mut Repo) -> Status {
        wait(repo, &mut None)
    }

    fn wait(repo: &mut Repo, failures: &mut Option<Vec<String>>) -> Status {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            for e in repo.poll() {
                match e {
                    Event::Status(st) => return st,
                    Event::Failed { error, .. } | Event::Error(error) => match failures {
                        Some(f) => f.push(error),
                        None => panic!("git error: {error}"),
                    },
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no status");
    }

    #[test]
    fn stage_unstage_commit_in_a_real_repo() {
        let dir = temp_dir("basic");
        init(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();

        let mut repo = Repo::open(&dir, Arc::new(|| {})).unwrap();
        let st = wait_status(&mut repo);
        assert!(st.initial);
        assert_eq!(st.unstaged[0].status, FileStatus::Untracked);

        repo.run(Op::Stage(vec![dir.join("a.txt")]));
        assert!(repo.busy().is_some());
        let st = wait_status(&mut repo);
        assert!(repo.busy().is_none());
        assert_eq!(st.staged[0].status, FileStatus::Added);
        // Unstaging works before the first commit too.
        repo.run(Op::Unstage(vec![dir.join("a.txt")]));
        assert_eq!(wait_status(&mut repo).staged.len(), 0);

        repo.run(Op::Commit { message: "first".into(), all: true, amend: false });
        let st = wait_status(&mut repo);
        assert_eq!(st.change_count(), 0);
        assert_eq!(st.branch.as_deref(), Some("main"));

        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        repo.send(Job::Refresh);
        let st = wait_status(&mut repo);
        assert_eq!(st.unstaged[0].status, FileStatus::Modified);
        assert_eq!(repo.file_status(&dir.join("a.txt")), Some(FileStatus::Modified));

        repo.send(Job::HeadContents(dir.join("a.txt")));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut head = None;
        while head.is_none() && Instant::now() < deadline {
            for e in repo.poll() {
                if let Event::HeadContents(_, text) = e {
                    head = Some(text);
                }
            }
        }
        let head = head.unwrap().unwrap();
        assert_eq!(head, "one\n");
        assert_eq!(line_changes(&head, "one\ntwo\n"), vec![LineChange::Added { start: 1, end: 2 }]);

        repo.run(Op::Discard(vec![dir.join("a.txt")]));
        assert_eq!(wait_status(&mut repo).change_count(), 0);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");

        // Amend keeps the message when none is given; undo brings the message back.
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        repo.run(Op::Commit { message: String::new(), all: true, amend: true });
        wait_status(&mut repo);
        assert_eq!(git::head_message(&dir).as_deref(), Some("first"));
        repo.run(Op::UndoLastCommit);
        let mut undone = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while undone.is_none() && Instant::now() < deadline {
            for e in repo.poll() {
                if let Event::Done { op: Op::UndoLastCommit, output } = e {
                    undone = Some(output);
                }
            }
        }
        assert_eq!(undone.as_deref(), Some("first"));
        let st = wait_status(&mut repo);
        assert!(st.initial);
        assert_eq!(st.staged.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branches_stashes_and_merge_conflicts() {
        let dir = temp_dir("branches");
        init(&dir);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "base"]);
        let mut repo = Repo::open(&dir, Arc::new(|| {})).unwrap();
        wait_status(&mut repo);

        repo.run(Op::CreateBranch { name: "topic".into(), from: None });
        let st = wait_status(&mut repo);
        assert_eq!(st.branch.as_deref(), Some("topic"));
        assert_eq!(repo.head_ref().map(|r| r.name.as_str()), Some("topic"));
        std::fs::write(dir.join("a.txt"), "topic\n").unwrap();
        repo.run(Op::Commit { message: "topic change".into(), all: true, amend: false });
        wait_status(&mut repo);

        repo.run(Op::Checkout { target: "main".into(), detached: false });
        assert_eq!(wait_status(&mut repo).branch.as_deref(), Some("main"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "base\n");

        // Stash a change, then bring it back.
        std::fs::write(dir.join("a.txt"), "main\n").unwrap();
        repo.run(Op::Stash { message: "mine".into(), untracked: false, staged: false });
        let st = wait_status(&mut repo);
        assert_eq!(st.change_count(), 0);
        assert_eq!(st.stashes.len(), 1);
        assert!(st.stashes[0].description.ends_with("mine"));
        repo.run(Op::StashPop(0));
        let st = wait_status(&mut repo);
        assert_eq!(st.stashes.len(), 0);
        repo.run(Op::Commit { message: "main change".into(), all: true, amend: false });
        wait_status(&mut repo);

        // Merging topic conflicts; the merge state and message show up in the status.
        repo.run(Op::Merge("topic".into()));
        let mut failures = Some(Vec::new());
        let st = wait(&mut repo, &mut failures);
        assert!(failures.unwrap()[0].contains("CONFLICT"));
        assert!(st.merging);
        assert_eq!(st.conflicts.len(), 1);
        assert!(st.merge_message.as_deref().unwrap().starts_with("Merge branch 'topic'"));
        repo.run(Op::AbortMerge);
        let st = wait_status(&mut repo);
        assert!(!st.merging);
        assert_eq!(st.change_count(), 0);

        repo.run(Op::RenameBranch("trunk".into()));
        assert_eq!(wait_status(&mut repo).branch.as_deref(), Some("trunk"));
        // An unmerged branch needs a forced delete.
        repo.run(Op::DeleteBranch { name: "topic".into(), force: false });
        let mut failures = Some(Vec::new());
        wait(&mut repo, &mut failures);
        assert_eq!(failures.unwrap().len(), 1);
        repo.run(Op::DeleteBranch { name: "topic".into(), force: true });
        let st = wait_status(&mut repo);
        assert!(st.refs.iter().all(|r| r.name != "topic"));

        repo.run(Op::CreateTag { name: "v1".into(), message: "first".into() });
        let st = wait_status(&mut repo);
        assert!(st.refs.iter().any(|r| r.kind == RefKind::Tag && r.name == "v1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_push_pull_and_sync_with_a_remote() {
        let base = temp_dir("remote");
        let (remote, a, b) = (base.join("remote.git"), base.join("a"), base.join("b"));
        run(&base, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
        std::fs::create_dir_all(&a).unwrap();
        init(&a);
        std::fs::write(a.join("f.txt"), "1\n").unwrap();
        run(&a, &["add", "-A"]);
        run(&a, &["commit", "-q", "-m", "one"]);

        let mut ra = Repo::open(&a, Arc::new(|| {})).unwrap();
        wait_status(&mut ra);
        ra.run(Op::AddRemote { name: "origin".into(), url: remote.to_str().unwrap().into() });
        let st = wait_status(&mut ra);
        assert_eq!(st.remotes[0].name, "origin");
        assert_eq!(st.upstream, None);
        ra.run(Op::Push { publish: Some("origin".into()), force: false });
        let st = wait_status(&mut ra);
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));

        clone(remote.to_str().unwrap(), &b).unwrap();
        run(&b, &["config", "user.email", "test@example.com"]);
        run(&b, &["config", "user.name", "Test"]);
        let mut rb = Repo::open(&b, Arc::new(|| {})).unwrap();
        wait_status(&mut rb);
        std::fs::write(b.join("g.txt"), "g\n").unwrap();
        rb.run(Op::Commit { message: "from b".into(), all: true, amend: false });
        assert_eq!(wait_status(&mut rb).ahead, 1);
        rb.run(Op::Push { publish: None, force: false });
        assert_eq!(wait_status(&mut rb).ahead, 0);

        // a is behind after a fetch; sync pulls b's commit and pushes a's.
        ra.run(Op::Fetch { remote: None, all: false, prune: false });
        assert_eq!(wait_status(&mut ra).behind, 1);
        std::fs::write(a.join("h.txt"), "h\n").unwrap();
        ra.run(Op::Commit { message: "from a".into(), all: true, amend: false });
        wait_status(&mut ra);
        ra.run(Op::Sync { rebase: false });
        let st = wait_status(&mut ra);
        assert_eq!((st.ahead, st.behind), (0, 0));
        assert!(a.join("g.txt").exists());

        rb.run(Op::Pull { rebase: true });
        wait_status(&mut rb);
        assert!(b.join("h.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
