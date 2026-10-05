//! Git operations through the `git` command line, with status parsing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl FileStatus {
    /// The letter we show next to the file.
    pub fn letter(self) -> char {
        match self {
            FileStatus::Modified => 'M',
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
            FileStatus::TypeChanged => 'T',
            FileStatus::Untracked => 'U',
            FileStatus::Conflicted => '!',
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            b'M' => FileStatus::Modified,
            b'A' => FileStatus::Added,
            b'D' => FileStatus::Deleted,
            b'R' => FileStatus::Renamed,
            b'C' => FileStatus::Copied,
            b'T' => FileStatus::TypeChanged,
            b'U' => FileStatus::Conflicted,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Absolute path.
    pub path: PathBuf,
    /// For renames/copies, the original path.
    pub orig_path: Option<PathBuf>,
    pub status: FileStatus,
}

/// A branch, remote branch or tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub kind: RefKind,
    /// Short name: "main", "origin/main", "v1.0".
    pub name: String,
    /// Short commit id.
    pub commit: String,
    /// Subject line of the commit it points to.
    pub subject: String,
    /// For local branches, the upstream ("origin/main").
    pub upstream: Option<String>,
    /// The checked-out branch.
    pub current: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    /// "WIP on main: 1234abc subject" or "On main: message".
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    /// The branch's upstream ("origin/main").
    pub upstream: Option<String>,
    /// The HEAD commit id (None before the first commit).
    pub head: Option<String>,
    /// Short commit id when HEAD is detached.
    pub detached_at: Option<String>,
    /// No commits yet.
    pub initial: bool,
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<Change>,
    /// Working tree changes, including untracked files.
    pub unstaged: Vec<Change>,
    pub conflicts: Vec<Change>,
    /// A merge is in progress (MERGE_HEAD exists), with git's prepared message.
    pub merging: bool,
    pub merge_message: Option<String>,
    /// A rebase is in progress.
    pub rebasing: bool,
    pub refs: Vec<Ref>,
    pub stashes: Vec<Stash>,
    pub remotes: Vec<Remote>,
}

impl Status {
    pub fn change_count(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.conflicts.len()
    }
}

fn git(root: &Path, args: &[&str]) -> Result<Output, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    run(cmd)
}

thread_local! {
    /// Set while running operations nobody asked for (automatic fetches): no prompts then.
    pub(crate) static NO_PROMPTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn run(mut cmd: Command) -> Result<Output, String> {
    // Credential prompts go to the editor's UI (see `askpass`); without it, git and ssh fail
    // rather than wait for an answer nobody can give.
    let prompts = !NO_PROMPTS.with(|n| n.get()) && !crate::askpass::env().is_empty();
    if prompts {
        cmd.envs(crate::askpass::env().iter().map(|(k, v)| (k, v)));
    } else {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd.stdin(std::process::Stdio::null())
        // Never block on an editor or pager, and keep output parseable.
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("could not run git: {e}"))
}

fn check(out: Output) -> Result<String, String> {
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        Err(msg)
    }
}

/// The repository root containing `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let s = check(out).ok()?;
    Some(PathBuf::from(s.trim()))
}

/// The `.git` directory of the repository at `root`.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let out = check(git(root, &["rev-parse", "--absolute-git-dir"]).ok()?).ok()?;
    Some(PathBuf::from(out.trim()))
}

pub fn status(root: &Path, git_dir: &Path) -> Result<Status, String> {
    let out = check(git(root, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"])?)?;
    let mut st = parse_status(root, &out);
    if st.branch.is_none() && !st.initial {
        st.detached_at = git_short_head(root);
    }
    st.merging = git_dir.join("MERGE_HEAD").exists();
    if st.merging {
        st.merge_message = std::fs::read_to_string(git_dir.join("MERGE_MSG")).ok().map(|m| clean_message(&m));
    }
    st.rebasing = git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    st.refs = refs(root).unwrap_or_default();
    st.stashes = stashes(root).unwrap_or_default();
    st.remotes = remotes(root).unwrap_or_default();
    Ok(st)
}

/// A commit message without git's `#` comment lines and trailing blank lines.
fn clean_message(m: &str) -> String {
    m.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n").trim_end().to_string()
}

/// Branches, remote branches and tags, most recently committed first.
pub fn refs(root: &Path) -> Result<Vec<Ref>, String> {
    let format = "--format=%(refname)%00%(objectname:short)%00%(upstream:short)%00%(HEAD)%00%(contents:subject)";
    let out = check(git(root, &["for-each-ref", "--sort=-committerdate", format, "refs/heads", "refs/remotes", "refs/tags"])?)?;
    Ok(parse_refs(&out))
}

pub fn parse_refs(out: &str) -> Vec<Ref> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let full = f.next()?;
            let (kind, name) = if let Some(n) = full.strip_prefix("refs/heads/") {
                (RefKind::Head, n)
            } else if let Some(n) = full.strip_prefix("refs/remotes/") {
                // "origin/HEAD" is an alias, not a branch.
                if n.ends_with("/HEAD") {
                    return None;
                }
                (RefKind::Remote, n)
            } else {
                (RefKind::Tag, full.strip_prefix("refs/tags/")?)
            };
            let commit = f.next()?.to_string();
            let upstream = f.next().filter(|u| !u.is_empty()).map(str::to_string);
            let current = f.next() == Some("*");
            let subject = f.next().unwrap_or_default().to_string();
            Some(Ref { kind, name: name.to_string(), commit, subject, upstream, current })
        })
        .collect()
}

pub fn stashes(root: &Path) -> Result<Vec<Stash>, String> {
    let out = check(git(root, &["stash", "list", "--format=%gd%x00%gs"])?)?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (sel, description) = line.split_once('\0')?;
            let index = sel.split_once('{')?.1.trim_end_matches('}').parse().ok()?;
            Some(Stash { index, description: description.to_string() })
        })
        .collect())
}

pub fn remotes(root: &Path) -> Result<Vec<Remote>, String> {
    let out = check(git(root, &["remote", "-v"])?)?;
    let mut remotes: Vec<Remote> = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), kind) = (parts.next(), parts.next(), parts.next()) else { continue };
        let i = match remotes.iter().position(|r| r.name == name) {
            Some(i) => i,
            None => {
                remotes.push(Remote { name: name.into(), fetch_url: String::new(), push_url: String::new() });
                remotes.len() - 1
            }
        };
        if kind == Some("(push)") {
            remotes[i].push_url = url.into();
        } else {
            remotes[i].fetch_url = url.into();
        }
    }
    Ok(remotes)
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub fn parse_status(root: &Path, out: &str) -> Status {
    let mut st = Status::default();
    let mut records = out.split('\0').peekable();
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(header) = rec.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => {
                    st.initial = value == "(initial)";
                    st.head = (!st.initial).then(|| value.to_string());
                }
                "branch.head" if value == "(detached)" => {}
                "branch.head" => st.branch = Some(value.to_string()),
                "branch.upstream" => st.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = rec.as_bytes()[0];
        let fields: Vec<&str> = rec.splitn(if kind == b'2' { 10 } else if kind == b'u' { 11 } else { 9 }, ' ').collect();
        match kind {
            b'1' | b'2' if fields.len() >= 9 => {
                let xy = fields[1].as_bytes();
                let path = root.join(fields.last().unwrap());
                // Renames carry the original path as the next NUL-separated record.
                let orig = if kind == b'2' { records.next().map(|p| root.join(p)) } else { None };
                if let Some(s) = FileStatus::from_code(xy[0]) {
                    st.staged.push(Change { path: path.clone(), orig_path: orig.clone(), status: s });
                }
                if let Some(s) = FileStatus::from_code(xy[1]) {
                    st.unstaged.push(Change { path, orig_path: orig, status: s });
                }
            }
            b'u' => {
                if let Some(p) = fields.last() {
                    st.conflicts.push(Change { path: root.join(p), orig_path: None, status: FileStatus::Conflicted });
                }
            }
            b'?' => st.unstaged.push(Change { path: root.join(&rec[2..]), orig_path: None, status: FileStatus::Untracked }),
            _ => {}
        }
    }
    st.staged.sort_by(|a, b| a.path.cmp(&b.path));
    st.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    st
}

fn git_short_head(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "--short", "HEAD"]).ok()?;
    check(out).ok().map(|s| s.trim().to_string())
}

fn rel_args(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()).collect()
}

fn run_with_paths(root: &Path, base: &[&str], paths: &[PathBuf]) -> Result<(), String> {
    let rel = rel_args(root, paths);
    let mut args: Vec<&str> = base.to_vec();
    args.push("--");
    args.extend(rel.iter().map(String::as_str));
    check(git(root, &args)?).map(|_| ())
}

/// Puts `contents` in the index as `path`'s staged version (keeping its file mode), like VS
/// Code's staging of selected ranges: `git hash-object -w`, then `git update-index --cacheinfo`.
pub fn stage_contents(root: &Path, path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    let rel = path.strip_prefix(root).map_err(|_| "file is outside the repository".to_string())?.to_string_lossy().replace('\\', "/");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "--stdin", &format!("--path={rel}")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    child.stdin.take().unwrap().write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
    let blob = check(child.wait_with_output().map_err(|e| e.to_string())?)?.trim().to_string();
    let mode = exec(root, &["ls-files", "-s", "--", &rel]).ok().and_then(|l| l.split_whitespace().next().map(str::to_string)).unwrap_or_else(|| "100644".into());
    exec(root, &["update-index", "--add", "--cacheinfo", &format!("{mode},{blob},{rel}")]).map(|_| ())
}

/// `git add` (also stages deletions).
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<(), String> {
    run_with_paths(root, &["add", "-A"], paths)
}

/// Removes paths from the index, keeping working tree changes.
pub fn unstage(root: &Path, paths: &[PathBuf], initial: bool) -> Result<(), String> {
    if initial {
        // No HEAD to restore from yet.
        run_with_paths(root, &["rm", "--cached", "-r", "-q"], paths)
    } else {
        run_with_paths(root, &["restore", "--staged"], paths)
    }
}

/// Reverts tracked files in the working tree to the index. (Untracked files are deleted
/// by the caller, after confirmation.)
pub fn discard(root: &Path, paths: &[PathBuf]) -> Result<(), String> {
    run_with_paths(root, &["restore", "--worktree"], paths)
}

/// Commits the index. With `all`, stages every change (including untracked files) first.
/// With `amend`, replaces the last commit (keeping its message if `message` is empty).
pub fn commit(root: &Path, message: &str, all: bool, amend: bool) -> Result<(), String> {
    if all {
        check(git(root, &["add", "-A"])?)?;
    }
    let mut args = vec!["commit", "-q"];
    if amend {
        args.push("--amend");
    }
    if amend && message.is_empty() {
        args.push("--no-edit");
    } else {
        args.extend(["-m", message]);
    }
    check(git(root, &args)?).map(|_| ())
}

/// Runs git with `args` in `root`, returning its output or its error message.
pub fn exec(root: &Path, args: &[&str]) -> Result<String, String> {
    check(git(root, args)?)
}

/// The full message of the HEAD commit.
pub fn head_message(root: &Path) -> Option<String> {
    exec(root, &["log", "-1", "--format=%B"]).ok().map(|m| m.trim_end().to_string())
}

/// `git clone <url> <dest>`.
pub fn clone(url: &str, dest: &Path) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--").arg(url).arg(dest);
    check(run(cmd)?).map(|_| ())
}

/// The folder name `git clone` would pick for `url` ("https://host/a/b.git" → "b").
pub fn clone_dir_name(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() { "repository".into() } else { name.to_string() }
}

/// The file's contents at HEAD, or None if it isn't tracked (or there is no HEAD).
pub fn head_contents(root: &Path, path: &Path) -> Option<String> {
    show(root, "HEAD", path)
}

/// A file's contents at a revision: "HEAD" or "" for the index (staged version).
/// A commit that changed a file (the Timeline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub hash: String,
    pub author: String,
    /// Commit time, seconds since the epoch.
    pub time: i64,
    pub subject: String,
}

/// The commits that changed `path`, newest first (following renames), at most `limit`.
pub fn file_log(root: &Path, path: &Path, limit: usize) -> Vec<LogEntry> {
    let Some(rel) = path.strip_prefix(root).ok().map(|r| r.to_string_lossy().replace('\\', "/")) else { return Vec::new() };
    let n = format!("-n{limit}");
    let Ok(out) = git(root, &["log", "--follow", &n, "--format=%H%x00%an%x00%at%x00%s", "--", &rel]) else { return Vec::new() };
    parse_log(&String::from_utf8_lossy(&out.stdout))
}

fn parse_log(text: &str) -> Vec<LogEntry> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\0');
            Some(LogEntry { hash: f.next()?.into(), author: f.next()?.into(), time: f.next()?.parse().ok()?, subject: f.next().unwrap_or("").into() })
        })
        .collect()
}

/// HEAD's history, with its upstream's (incoming commits), newest first, at most `limit`.
pub fn graph_log(root: &Path, limit: usize) -> Vec<crate::graph::GraphCommit> {
    let n = format!("-n{limit}");
    let mut args = vec!["log", "--topo-order", "--decorate=full", &n, "--format=%H%x00%P%x00%D%x00%an%x00%at%x00%s", "HEAD"];
    let upstream = git(root, &["rev-parse", "--verify", "-q", "@{upstream}"]).ok().filter(|o| o.status.success());
    if upstream.is_some() {
        args.push("@{upstream}");
    }
    match git(root, &args) {
        Ok(out) if out.status.success() => crate::graph::parse_commits(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// The files `hash` changed (against its first parent), with their status letters.
pub fn commit_files(root: &Path, hash: &str) -> Vec<(char, PathBuf)> {
    let has_parent = git(root, &["rev-parse", "--verify", "-q", &format!("{hash}^")]).is_ok_and(|o| o.status.success());
    let out = if has_parent {
        git(root, &["diff", "--name-status", "-M", &format!("{hash}^"), hash])
    } else {
        git(root, &["diff-tree", "--root", "--no-commit-id", "--name-status", "-r", hash])
    };
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let status = f.next()?.chars().next()?;
            // Renames list the old path, then the new one.
            let path = f.last()?;
            Some((status, root.join(path)))
        })
        .collect()
}

/// A commit's full hash and the refs pointing at it (`%D`: "HEAD -> main, origin/main").
pub fn commit_refs(root: &Path, rev: &str) -> Option<(String, String)> {
    let out = exec(root, &["log", "-1", "--format=%H%x00%D", rev, "--"]).ok()?;
    let (hash, refs) = out.trim_end().split_once('\0')?;
    Some((hash.to_string(), refs.to_string()))
}

pub fn show(root: &Path, rev: &str, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    let out = git(root, &["show", &format!("{rev}:{rel}")]).ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_lines() {
        let log = parse_log("abc\0Ann\01700000000\0Fix: thing\ndef\0Bo\01690000000\0\n");
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "Fix: thing");
        assert_eq!(log[1].time, 1690000000);
    }

    #[test]
    fn parses_porcelain_v2() {
        let root = Path::new("/repo");
        let out = [
            "# branch.oid 1234abcd",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaa bbb src/lib.rs",
            "1 A. N... 000000 100644 100644 000 ccc new file.rs",
            "1 MM N... 100644 100644 100644 aaa bbb both.rs",
            "2 R. N... 100644 100644 100644 aaa aaa R100 renamed.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
            "? untracked.txt",
            "",
        ]
        .join("\0");
        let st = parse_status(root, &out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let staged: Vec<_> = st.staged.iter().map(|c| (c.path.to_str().unwrap(), c.status)).collect();
        assert_eq!(
            staged,
            vec![
                ("/repo/both.rs", FileStatus::Modified),
                ("/repo/new file.rs", FileStatus::Added),
                ("/repo/renamed.rs", FileStatus::Renamed),
            ]
        );
        assert_eq!(st.staged[2].orig_path.as_deref(), Some(Path::new("/repo/old.rs")));
        let unstaged: Vec<_> = st.unstaged.iter().map(|c| (c.path.to_str().unwrap(), c.status)).collect();
        assert_eq!(
            unstaged,
            vec![
                ("/repo/both.rs", FileStatus::Modified),
                ("/repo/src/lib.rs", FileStatus::Modified),
                ("/repo/untracked.txt", FileStatus::Untracked),
            ]
        );
        assert_eq!(st.conflicts.len(), 1);
        assert_eq!(st.change_count(), 7);
    }

    #[test]
    fn parses_refs_and_clone_names() {
        let out = [
            "refs/heads/main\0abc1234\0origin/main\0*\0Add things",
            "refs/heads/topic\0def5678\0\0 \0Work",
            "refs/remotes/origin/HEAD\0abc1234\0\0 \0Add things",
            "refs/remotes/origin/main\0abc1234\0\0 \0Add things",
            "refs/tags/v1.0\0aaa0000\0\0 \0Release",
        ]
        .join("\n");
        let refs = parse_refs(&out);
        let names: Vec<_> = refs.iter().map(|r| (r.kind, r.name.as_str(), r.current)).collect();
        assert_eq!(
            names,
            vec![
                (RefKind::Head, "main", true),
                (RefKind::Head, "topic", false),
                (RefKind::Remote, "origin/main", false),
                (RefKind::Tag, "v1.0", false),
            ]
        );
        assert_eq!(refs[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(refs[1].upstream, None);
        assert_eq!(clone_dir_name("https://github.com/rust-lang/rust.git"), "rust");
        assert_eq!(clone_dir_name("git@github.com:me/tool.git/"), "tool");
        assert_eq!(clone_dir_name("/local/path/repo"), "repo");
        assert_eq!(clean_message("Merge branch 'x'\n\n# Conflicts:\n#\ta.txt\n"), "Merge branch 'x'");
    }
}
