//! Glob patterns (`*.rs`, `src/**/test_*`, `[abc]?.txt`) and `.gitignore` rules.

use std::path::Path;

use regex::Regex;

/// Translates a glob to a regex over `/`-separated relative paths.
/// `*` and `?` stay within a path segment; `**` crosses segments.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `**/` matches zero or more directories; a trailing `**` matches everything.
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                // Character class; `!` negates like in shells.
                let close = chars[i + 1..].iter().position(|&c| c == ']').map(|p| p + i + 1);
                match close {
                    Some(end) if end > i + 1 => {
                        let mut class: String = chars[i + 1..end].iter().collect();
                        if class.starts_with('!') {
                            class.replace_range(0..1, "^");
                        }
                        re.push('[');
                        re.push_str(&class.replace('\\', "\\\\"));
                        re.push(']');
                        i = end + 1;
                        continue;
                    }
                    _ => re.push_str("\\["),
                }
            }
            '{' => {
                // Brace alternatives: {rs,toml}
                if let Some(end) = chars[i + 1..].iter().position(|&c| c == '}').map(|p| p + i + 1) {
                    let alts: String = chars[i + 1..end].iter().collect();
                    let parts: Vec<String> = alts.split(',').map(|a| glob_to_regex(a)[1..].trim_end_matches('$').to_string()).collect();
                    re.push_str(&format!("(?:{})", parts.join("|")));
                    i = end + 1;
                    continue;
                }
                re.push_str("\\{");
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    re
}

/// A set of globs, as typed in the "files to include/exclude" fields.
#[derive(Clone, Debug, Default)]
pub struct GlobSet {
    patterns: Vec<Regex>,
}

impl GlobSet {
    /// Parses a comma-separated list. Patterns without a `/` match at any depth, and a
    /// bare folder name also matches everything inside it.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut patterns = Vec::new();
        for raw in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let raw = raw.trim_start_matches("./");
            let glob = if raw.contains('/') { raw.trim_start_matches('/').to_string() } else { format!("**/{raw}") };
            for g in [glob.clone(), format!("{}/**", glob.trim_end_matches('/'))] {
                patterns.push(Regex::new(&glob_to_regex(&g)).map_err(|e| e.to_string())?);
            }
        }
        Ok(Self { patterns })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// `rel` is a path relative to the search root, using `/` separators.
    pub fn matches(&self, rel: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(rel))
    }
}

struct Rule {
    regex: Regex,
    negate: bool,
    dir_only: bool,
}

/// The rules from one `.gitignore` file, relative to the directory containing it.
pub struct Gitignore {
    rules: Vec<Rule>,
}

impl Gitignore {
    pub fn parse(content: &str) -> Self {
        let mut rules = Vec::new();
        for line in content.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negate, pat) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line.strip_prefix('\\').unwrap_or(line)),
            };
            let dir_only = pat.ends_with('/');
            let pat = pat.trim_end_matches('/');
            // A slash anywhere but the end anchors the pattern to this directory.
            let anchored = pat.contains('/');
            let pat = pat.trim_start_matches('/');
            let glob = if anchored { pat.to_string() } else { format!("**/{pat}") };
            if let Ok(regex) = Regex::new(&glob_to_regex(&glob)) {
                rules.push(Rule { regex, negate, dir_only });
            }
        }
        Self { rules }
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(dir.join(".gitignore")).ok()?;
        Some(Self::parse(&content))
    }

    /// Some(true) if ignored, Some(false) if explicitly re-included, None if no rule applies.
    /// The last matching rule wins, as in git.
    pub fn check(&self, rel: &str, is_dir: bool) -> Option<bool> {
        self.rules
            .iter()
            .rev()
            .find(|r| (!r.dir_only || is_dir) && r.regex.is_match(rel))
            .map(|r| !r.negate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(glob: &str, path: &str) -> bool {
        Regex::new(&glob_to_regex(glob)).unwrap().is_match(path)
    }

    #[test]
    fn globs() {
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "src/main.rs"));
        assert!(m("**/*.rs", "src/main.rs"));
        assert!(m("**/*.rs", "main.rs"));
        assert!(m("src/**", "src/a/b.txt"));
        assert!(m("file?.[ch]", "file1.c"));
        assert!(!m("file?.[!ch]", "file1.c"));
        assert!(m("*.{rs,toml}", "Cargo.toml"));
    }

    #[test]
    fn glob_sets() {
        let set = GlobSet::parse("*.md, target").unwrap();
        assert!(set.matches("README.md"));
        assert!(set.matches("docs/guide.md"));
        assert!(set.matches("target/debug/app"));
        assert!(set.matches("crates/x/target/foo"));
        assert!(!set.matches("src/main.rs"));
        let anchored = GlobSet::parse("crates/app/**").unwrap();
        assert!(anchored.matches("crates/app/src/main.rs"));
        assert!(!anchored.matches("crates/lsp/src/lib.rs"));
    }

    #[test]
    fn gitignore_rules() {
        let gi = Gitignore::parse("# comment\ntarget/\n*.log\n!keep.log\n/build\ndocs/*.tmp\n");
        assert_eq!(gi.check("target", true), Some(true));
        assert_eq!(gi.check("target", false), None); // dir-only rule
        assert_eq!(gi.check("sub/target", true), Some(true));
        assert_eq!(gi.check("x/debug.log", false), Some(true));
        assert_eq!(gi.check("keep.log", false), Some(false));
        assert_eq!(gi.check("build", true), Some(true));
        assert_eq!(gi.check("src/build", true), None); // anchored to the root
        assert_eq!(gi.check("docs/a.tmp", false), Some(true));
        assert_eq!(gi.check("main.rs", false), None);
    }
}
