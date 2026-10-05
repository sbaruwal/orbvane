//! Reacting to changes on disk in the open folder (`fswatch`, macOS FSEvents): the Explorer
//! re-reads the folders it shows, open files without unsaved changes reload, Go to File's
//! list is rebuilt, and git status is refreshed soon (at most once a second).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Workbench;

/// Git status is refreshed at most this often because of file changes.
const GIT_THROTTLE: Duration = Duration::from_secs(1);

pub(super) struct Watching {
    watcher: fswatch::Watcher,
    /// The folder as FSEvents reports it (symlinks resolved) and as the tree shows it.
    real_root: PathBuf,
    root: PathBuf,
}

/// Whether a change at `path` can change git status: not in build output, and in `.git`
/// only the index, HEAD and refs.
fn matters_to_git(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    if parts.iter().any(|c| matches!(*c, "target" | "node_modules")) {
        return false;
    }
    match parts.iter().position(|c| *c == ".git") {
        Some(i) => matches!(parts.get(i + 1), Some(&"index" | &"HEAD" | &"refs" | &"MERGE_HEAD")),
        None => true,
    }
}

impl Workbench {
    /// Starts watching the workspace's folders (stops watching the previous ones).
    pub(super) fn start_watching(&mut self) {
        self.watching.clear();
        for root in self.folders() {
            let real_root = root.canonicalize().unwrap_or_else(|_| root.clone());
            let waker = self.waker.clone();
            match fswatch::Watcher::new(&real_root, std::sync::Arc::new(move || waker())) {
                Ok(watcher) => self.watching.push(Watching { watcher, real_root, root }),
                Err(e) => eprintln!("watching {}: {e}", root.display()),
            }
        }
    }


    /// Applies the changes reported since the last frame. Called every frame.
    pub(super) fn watch_tick(&mut self) {
        let now = Instant::now();
        if self.git_nudge.is_some_and(|t| now >= t) {
            self.git_nudge = None;
            self.refresh_scm();
        }
        let mut paths: Vec<PathBuf> = self
            .watching
            .iter()
            .flat_map(|w| w.watcher.poll().into_iter().map(|p| p.strip_prefix(&w.real_root).map(|r| w.root.join(r)).unwrap_or(p)))
            .collect();
        if paths.is_empty() {
            return;
        }
        paths.sort();
        paths.dedup();
        // Git status only matters for the repository the Source Control view shows.
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        if let Some(tree) = &mut self.tree {
            if paths.iter().any(|p| p.parent().is_some_and(|d| tree.has_loaded(d))) {
                tree.refresh();
            }
        }
        self.palette_files = None;
        self.testing_files_changed(&paths);
        for p in &paths {
            self.reload_clean_doc(p);
        }
        if self.repo.is_some() && self.git_nudge.is_none() && paths.iter().any(|p| p.starts_with(&root) && matters_to_git(&root, p)) {
            let soonest = self.scm.last_refresh.map_or(now, |t| t + GIT_THROTTLE);
            self.git_nudge = Some(soonest.max(now + Duration::from_millis(200)));
        }
    }

    pub(super) fn watch_deadline(&self) -> Option<Instant> {
        self.git_nudge
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_relevant_changes() {
        let root = Path::new("/p");
        assert!(matters_to_git(root, Path::new("/p/src/main.rs")));
        assert!(!matters_to_git(root, Path::new("/p/target/debug/x.o")));
        assert!(matters_to_git(root, Path::new("/p/.git/index")));
        assert!(matters_to_git(root, Path::new("/p/.git/refs/heads/main")));
        assert!(!matters_to_git(root, Path::new("/p/.git/objects/ab/cdef")));
    }
}
