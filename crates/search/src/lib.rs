//! Project-wide text search: walks a folder (respecting .gitignore), searches files in
//! parallel on background threads and streams results back.

mod glob;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

pub use glob::{GlobSet, Gitignore};
use regex::{Regex, RegexBuilder};

/// Stop after this many matches, like `search.maxResults`.
pub const MAX_RESULTS: usize = 20_000;
const MAX_FILE_SIZE: u64 = 20 * 1024 * 1024;
const MAX_PREVIEW: usize = 400;

/// Always skipped, matching the default `files.exclude` and `search.exclude`.
const DEFAULT_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", "CVS", ".DS_Store", "node_modules", "bower_components"];

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Comma-separated globs ("files to include" / "files to exclude").
    pub include: String,
    pub exclude: String,
    /// Honor .gitignore files.
    pub use_ignore_files: bool,
}

impl Query {
    pub fn compile(&self) -> Result<Regex, String> {
        let pat = if self.regex { self.pattern.clone() } else { regex::escape(&self.pattern) };
        let pat = if self.whole_word { format!(r"\b(?:{pat})\b") } else { pat };
        RegexBuilder::new(&pat)
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            .build()
            .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").trim().to_string())
    }
}

/// One match: a 0-based line and a byte range within that line.
#[derive(Clone, Debug)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /// The line's text (possibly shortened around the match for very long lines);
    /// `preview_start`/`preview_end` locate the match within it.
    pub preview: String,
    pub preview_start: usize,
    pub preview_end: usize,
}

#[derive(Clone, Debug)]
pub struct FileMatches {
    pub path: PathBuf,
    pub matches: Vec<Match>,
}

/// A running search. Dropping it cancels the search.
pub struct Search {
    rx: Receiver<FileMatches>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    pub files_searched: Arc<AtomicUsize>,
    pub limit_hit: Arc<AtomicBool>,
}

impl Search {
    pub fn start(root: &Path, query: &Query, waker: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        Self::start_in(&[root.to_path_buf()], query, waker)
    }

    /// Searches several folders (a multi-root workspace's) as one search; include and
    /// exclude patterns apply within each folder.
    pub fn start_in(roots: &[PathBuf], query: &Query, waker: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        let regex = query.compile()?;
        let include = GlobSet::parse(&query.include)?;
        let exclude = GlobSet::parse(&query.exclude)?;
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let files_searched = Arc::new(AtomicUsize::new(0));
        let limit_hit = Arc::new(AtomicBool::new(false));
        let roots = roots.to_vec();
        let use_ignore = query.use_ignore_files;
        let (c, d, fs, lh) = (cancel.clone(), done.clone(), files_searched.clone(), limit_hit.clone());
        thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let files: Vec<PathBuf> = roots.iter().flat_map(|root| walk(root, use_ignore, &include, &exclude, &c)).collect();
                let files = Arc::new(files);
                let next = Arc::new(AtomicUsize::new(0));
                let total = Arc::new(AtomicUsize::new(0));
                let workers = thread::available_parallelism().map_or(4, |n| n.get()).min(8);
                let handles: Vec<_> = (0..workers)
                    .map(|_| {
                        let (files, next, total, tx) = (files.clone(), next.clone(), total.clone(), tx.clone());
                        let (regex, c, fs, lh, waker) = (regex.clone(), c.clone(), fs.clone(), lh.clone(), waker.clone());
                        thread::spawn(move || loop {
                            if c.load(Ordering::Relaxed) {
                                return;
                            }
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(path) = files.get(i) else { return };
                            fs.fetch_add(1, Ordering::Relaxed);
                            let Some(matches) = search_file(path, &regex) else { continue };
                            let n = matches.len();
                            if total.fetch_add(n, Ordering::Relaxed) + n >= MAX_RESULTS {
                                lh.store(true, Ordering::Relaxed);
                                c.store(true, Ordering::Relaxed);
                            }
                            if tx.send(FileMatches { path: path.clone(), matches }).is_err() {
                                return;
                            }
                            waker();
                        })
                    })
                    .collect();
                for h in handles {
                    let _ = h.join();
                }
                d.store(true, Ordering::SeqCst);
                waker();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { rx, cancel, done, files_searched, limit_hit })
    }

    /// Results that arrived since the last call.
    pub fn poll(&self) -> Vec<FileMatches> {
        self.rx.try_iter().collect()
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Lists files under `root`, honoring default excludes, .gitignore files and the globs.
fn walk(root: &Path, use_ignore: bool, include: &GlobSet, exclude: &GlobSet, cancel: &AtomicBool) -> Vec<PathBuf> {
    // Each directory on the stack carries the .gitignore rules in effect for it, nearest last.
    let mut stack: Vec<(PathBuf, Vec<(PathBuf, Arc<Gitignore>)>)> = vec![(root.to_path_buf(), Vec::new())];
    let mut out = Vec::new();
    while let Some((dir, mut ignores)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if use_ignore {
            if let Some(gi) = Gitignore::load(&dir) {
                ignores.push((dir.clone(), Arc::new(gi)));
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if DEFAULT_EXCLUDES.contains(&name.as_ref()) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            let is_dir = ft.is_dir() || (ft.is_symlink() && path.is_dir());
            let rel = relative(root, &path);
            // Nearest .gitignore decides first; parents only if it has no opinion.
            let ignored = ignores.iter().rev().find_map(|(base, gi)| gi.check(&relative(base, &path), is_dir)).unwrap_or(false);
            if ignored || exclude.matches(&rel) {
                continue;
            }
            if is_dir {
                if !ft.is_symlink() {
                    stack.push((path, ignores.clone()));
                }
            } else if include.is_empty() || include.matches(&rel) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn search_file(path: &Path, regex: &Regex) -> Option<Vec<Match>> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_SIZE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // Treat files with a NUL byte near the start as binary, like git.
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let matches = search_text(&text, regex);
    (!matches.is_empty()).then_some(matches)
}

/// Finds all matches in `text`, line by line.
pub fn search_text(text: &str, regex: &Regex) -> Vec<Match> {
    let mut out = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for m in regex.find_iter(line) {
            if m.start() == m.end() {
                continue; // skip empty matches (e.g. `^` or `x*`)
            }
            let (preview, ps, pe) = preview(line, m.start(), m.end());
            out.push(Match { line: line_no, start: m.start(), end: m.end(), preview, preview_start: ps, preview_end: pe });
        }
    }
    out
}

/// Shortens very long lines to a window around the match (on char boundaries).
fn preview(line: &str, start: usize, end: usize) -> (String, usize, usize) {
    let trimmed = line.trim_start();
    let lead = line.len() - trimmed.len();
    if line.len() - lead <= MAX_PREVIEW && start >= lead {
        return (trimmed.to_string(), start - lead, end - lead);
    }
    let mut from = start.saturating_sub(40).max(lead.min(start));
    while !line.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = (from + MAX_PREVIEW).min(line.len()).max(end);
    while !line.is_char_boundary(to) {
        to += 1;
    }
    let prefix = if from > lead { "…" } else { "" };
    let text = format!("{prefix}{}", &line[from..to]);
    let shift = prefix.len();
    (text, start - from + shift, end - from + shift)
}

/// Replaces every match in `text` (line by line, like the search). In regex mode the
/// replacement may use `$1`-style group references. Returns the new text and the count.
pub fn replace_text(text: &str, regex: &Regex, replacement: &str, is_regex: bool) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let (body, cr) = match line.strip_suffix('\r') {
            Some(b) => (b, "\r"),
            None => (line, ""),
        };
        count += regex.find_iter(body).filter(|m| m.start() != m.end()).count();
        let replaced = if is_regex {
            regex.replace_all(body, replacement)
        } else {
            regex.replace_all(body, regex::NoExpand(replacement))
        };
        out.push_str(&replaced);
        out.push_str(cr);
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn q(pattern: &str) -> Query {
        Query { pattern: pattern.into(), use_ignore_files: true, ..Default::default() }
    }

    #[test]
    fn query_options() {
        let text = "Foo foo food\nbar";
        assert_eq!(search_text(text, &q("foo").compile().unwrap()).len(), 3);
        assert_eq!(search_text(text, &Query { case_sensitive: true, ..q("Foo") }.compile().unwrap()).len(), 1);
        assert_eq!(search_text(text, &Query { whole_word: true, ..q("foo") }.compile().unwrap()).len(), 2);
        let re = Query { regex: true, ..q(r"fo+d?\b") }.compile().unwrap();
        assert_eq!(search_text(text, &re).len(), 3);
        assert!(Query { regex: true, ..q("(") }.compile().is_err());
        // Literal mode escapes regex syntax.
        assert_eq!(search_text("a.b axb", &q("a.b").compile().unwrap()).len(), 1);
    }

    #[test]
    fn match_positions() {
        let m = &search_text("line one\n    let x = needle;\n", &q("needle").compile().unwrap())[0];
        assert_eq!((m.line, m.start, m.end), (1, 12, 18));
        assert_eq!(m.preview, "let x = needle;");
        assert_eq!(&m.preview[m.preview_start..m.preview_end], "needle");
    }

    #[test]
    fn replaces_with_groups() {
        let re = Query { regex: true, ..q(r"(\w+)@(\w+)") }.compile().unwrap();
        let (out, n) = replace_text("a@b\r\nc@d\nno", &re, "$2@$1", true);
        assert_eq!((out.as_str(), n), ("b@a\r\nd@c\nno", 2));
        let lit = q("$x").compile().unwrap();
        assert_eq!(replace_text("cost $x", &lit, "$1", false).0, "cost $1");
    }

    #[test]
    fn searches_a_tree_respecting_gitignore() {
        let dir = std::env::temp_dir().join(format!("orbvane-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(dir.join("src/nested/.gitignore"), "secret.txt\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn needle() {}\n").unwrap();
        std::fs::write(dir.join("src/nested/a.txt"), "needle\nneedle\n").unwrap();
        std::fs::write(dir.join("src/nested/secret.txt"), "needle").unwrap();
        std::fs::write(dir.join("target/out.rs"), "needle").unwrap();
        std::fs::write(dir.join("debug.log"), "needle").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "needle").unwrap();
        std::fs::write(dir.join("image.bin"), b"needle\0\x01").unwrap();

        let run = |query: Query| {
            let s = Search::start(&dir, &query, Arc::new(|| {})).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut results = Vec::new();
            while !s.is_done() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            results.extend(s.poll());
            let mut files: Vec<String> = results.iter().map(|f| relative(&dir, &f.path)).collect();
            files.sort();
            (files, results.iter().map(|f| f.matches.len()).sum::<usize>())
        };
        assert_eq!(run(q("needle")), (vec!["src/main.rs".into(), "src/nested/a.txt".into()], 3));
        let no_ignore = run(Query { use_ignore_files: false, ..q("needle") });
        assert_eq!(no_ignore.0.len(), 5, "{:?}", no_ignore.0); // + secret, target, log (still no node_modules/binary)
        assert_eq!(run(Query { include: "*.rs".into(), ..q("needle") }).0, vec!["src/main.rs".to_string()]);
        assert_eq!(run(Query { exclude: "nested".into(), ..q("needle") }).0, vec!["src/main.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
