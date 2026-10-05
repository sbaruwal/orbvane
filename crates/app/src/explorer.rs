//! File tree model for the Explorer view. Directories load lazily on first expand.

use std::fs;
use std::path::{Path, PathBuf};

/// Entries hidden from the tree, matching the default `files.exclude`.
const EXCLUDED: &[&str] = &[".git", ".svn", ".hg", "CVS", ".DS_Store", "Thumbs.db"];

struct Node {
    name: String,
    path: PathBuf,
    is_dir: bool,
    expanded: bool,
    children: Option<Vec<Node>>,
}

impl Node {
    fn new(path: PathBuf, is_dir: bool) -> Self {
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into());
        Self { name, path, is_dir, expanded: false, children: None }
    }

    fn load(&mut self) {
        if self.children.is_some() {
            return;
        }
        let mut children: Vec<Node> = fs::read_dir(&self.path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !EXCLUDED.contains(&e.file_name().to_string_lossy().as_ref()))
            .map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    || (e.file_type().map(|t| t.is_symlink()).unwrap_or(false) && e.path().is_dir());
                Node::new(e.path(), is_dir)
            })
            .collect();
        children.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        self.children = Some(children);
    }

    fn flatten(&self, depth: usize, out: &mut Vec<Row>) {
        for child in self.children.iter().flatten() {
            out.push(Row {
                name: child.name.clone(),
                path: child.path.clone(),
                depth,
                is_dir: child.is_dir,
                expanded: child.expanded,
                root: false,
            });
            if child.is_dir && child.expanded {
                child.flatten(depth + 1, out);
            }
        }
    }

    fn find(&self, path: &Path) -> Option<&Node> {
        if self.path == path {
            return Some(self);
        }
        if !path.starts_with(&self.path) {
            return None;
        }
        self.children.as_ref()?.iter().find_map(|c| c.find(path))
    }

    fn find_mut(&mut self, path: &Path) -> Option<&mut Node> {
        if self.path == path {
            return Some(self);
        }
        if !path.starts_with(&self.path) {
            return None;
        }
        self.children.as_mut()?.iter_mut().find_map(|c| c.find_mut(path))
    }
}

/// Rebuilds `node` from disk, re-expanding whatever was expanded in `prev`.
fn restore(node: &mut Node, prev: &Node) {
    node.expanded = prev.expanded;
    let Some(prev_children) = &prev.children else { return };
    node.load();
    for child in node.children.iter_mut().flatten() {
        if let Some(p) = prev_children.iter().find(|p| p.path == child.path) {
            if p.is_dir && p.children.is_some() {
                restore(child, p);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// A workspace folder (only shown as a row when the workspace has several).
    pub root: bool,
}

/// The Explorer's folders. With one folder its contents are the top level; with several
/// (a multi-root workspace) each folder is a top-level row.
pub struct FileTree {
    roots: Vec<Node>,
    pub rows: Vec<Row>,
    pub selected: Option<usize>,
    pub scroll: f32,
}

fn root_node(path: &Path, name: Option<&str>) -> Node {
    let mut node = Node::new(path.to_path_buf(), true);
    if let Some(name) = name {
        node.name = name.to_string();
    }
    node.expanded = true;
    node.load();
    node
}

impl FileTree {
    pub fn new(root: &Path) -> Self {
        Self::with_roots(&[(root.to_path_buf(), None)])
    }

    /// A tree of several folders, each with an optional display name.
    pub fn with_roots(roots: &[(PathBuf, Option<String>)]) -> Self {
        let roots = roots.iter().map(|(p, n)| root_node(p, n.as_deref())).collect();
        let mut tree = Self { roots, rows: Vec::new(), selected: None, scroll: 0.0 };
        tree.rebuild();
        tree
    }

    pub fn root_name(&self) -> &str {
        &self.roots[0].name
    }

    /// The first folder.
    pub fn root_path(&self) -> &Path {
        &self.roots[0].path
    }

    /// Every folder.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.path.clone()).collect()
    }

    /// The folders' names as shown (a workspace can rename them).
    pub fn root_names(&self) -> Vec<String> {
        self.roots.iter().map(|r| r.name.clone()).collect()
    }

    /// Adds a folder at the end (or renames it if it's there already).
    pub fn add_root(&mut self, path: &Path, name: Option<&str>) {
        match self.roots.iter_mut().find(|r| r.path == path) {
            Some(r) => r.name = name.map_or_else(|| Node::new(path.to_path_buf(), true).name, str::to_string),
            None => self.roots.push(root_node(path, name)),
        }
        self.rebuild();
    }

    /// Removes a folder; the last one stays.
    pub fn remove_root(&mut self, path: &Path) {
        if self.roots.len() > 1 {
            self.roots.retain(|r| r.path != path);
            self.rebuild();
        }
    }

    fn multi(&self) -> bool {
        self.roots.len() > 1
    }

    fn find_mut(&mut self, path: &Path) -> Option<&mut Node> {
        self.roots.iter_mut().find_map(|r| r.find_mut(path))
    }

    fn find(&self, path: &Path) -> Option<&Node> {
        self.roots.iter().find_map(|r| r.find(path))
    }

    /// The folder `path` is in (the innermost, if folders are nested).
    fn root_of(&self, path: &Path) -> Option<usize> {
        (0..self.roots.len()).filter(|&i| path.starts_with(&self.roots[i].path)).max_by_key(|&i| self.roots[i].path.components().count())
    }

    fn rebuild(&mut self) {
        let selected = self.selected.and_then(|i| self.rows.get(i)).map(|r| r.path.clone());
        self.rows.clear();
        if self.multi() {
            for root in &self.roots {
                self.rows.push(Row { name: root.name.clone(), path: root.path.clone(), depth: 0, is_dir: true, expanded: root.expanded, root: true });
                if root.expanded {
                    root.flatten(1, &mut self.rows);
                }
            }
        } else {
            self.roots[0].flatten(0, &mut self.rows);
        }
        self.selected = selected.and_then(|p| self.rows.iter().position(|r| r.path == p));
    }

    pub fn toggle(&mut self, index: usize) {
        let Some(row) = self.rows.get(index) else { return };
        let path = row.path.clone();
        if let Some(node) = self.find_mut(&path) {
            if node.is_dir {
                node.expanded = !node.expanded;
                if node.expanded {
                    node.load();
                }
            }
        }
        self.rebuild();
    }

    pub fn set_expanded(&mut self, index: usize, expanded: bool) {
        if self.rows.get(index).is_some_and(|r| r.is_dir && r.expanded != expanded) {
            self.toggle(index);
        }
    }

    /// Index of the parent directory row for `index`, if any.
    pub fn parent(&self, index: usize) -> Option<usize> {
        let depth = self.rows.get(index)?.depth;
        (0..index).rev().find(|&i| self.rows[i].depth + 1 == depth)
    }

    /// Expands the directories leading to `path` and selects it.
    pub fn reveal(&mut self, path: &Path) {
        if let Some(i) = self.root_of(path) {
            let root = &mut self.roots[i];
            root.expanded = true;
            let mut dir = root.path.clone();
            if let Ok(rel) = path.strip_prefix(&root.path) {
                let comps: Vec<_> = rel.components().collect();
                for comp in comps.iter().take(comps.len().saturating_sub(1)) {
                    dir.push(comp);
                    if let Some(node) = root.find_mut(&dir) {
                        node.expanded = true;
                        node.load();
                    }
                }
            }
        }
        self.rebuild();
        self.selected = self.rows.iter().position(|r| r.path == path);
    }

    /// Expands these folders (absolute paths; missing ones are skipped).
    pub fn expand_paths(&mut self, paths: &[PathBuf]) {
        let mut paths: Vec<&PathBuf> = paths.iter().collect();
        // Parents first, so their children are loaded when we get to them.
        paths.sort_by_key(|p| p.components().count());
        for path in paths {
            if let Some(node) = self.find_mut(path).filter(|n| n.is_dir) {
                node.expanded = true;
                node.load();
            }
        }
        self.rebuild();
    }

    /// The expanded folders (for the session), workspace folders included.
    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        fn walk(node: &Node, out: &mut Vec<PathBuf>) {
            for c in node.children.iter().flatten().filter(|c| c.is_dir && c.expanded) {
                out.push(c.path.clone());
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        for r in &self.roots {
            if r.expanded {
                out.push(r.path.clone());
            }
            walk(r, &mut out);
        }
        out
    }

    /// Whether the tree has read folder `dir`'s entries (so a change in it should show).
    pub fn has_loaded(&self, dir: &Path) -> bool {
        self.find(dir).is_some_and(|n| n.is_dir && n.children.is_some())
    }

    /// Collapse Folders in Explorer: every folder closes (a single root stays open; the
    /// folders of a multi-root workspace close too).
    pub fn collapse_all(&mut self) {
        fn close(node: &mut Node) {
            for c in node.children.iter_mut().flatten() {
                c.expanded = false;
                close(c);
            }
        }
        let multi = self.multi();
        for r in &mut self.roots {
            close(r);
            if multi {
                r.expanded = false;
            }
        }
        self.scroll = 0.0;
        self.rebuild();
    }

    /// Re-reads the tree from disk, keeping expanded folders expanded.
    pub fn refresh(&mut self) {
        for r in &mut self.roots {
            let mut fresh = Node::new(r.path.clone(), true);
            fresh.name = r.name.clone();
            restore(&mut fresh, r);
            *r = fresh;
        }
        self.rebuild();
    }
}

/// Recursively lists files under `root` for Quick Open, skipping heavy build/vendor directories.
pub fn walk_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[".git", "target", "node_modules", ".venv", "venv", "__pycache__", "dist", "build"];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP.contains(&name.as_ref()) || EXCLUDED.contains(&name.as_ref()) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                out.push(entry.path());
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}
