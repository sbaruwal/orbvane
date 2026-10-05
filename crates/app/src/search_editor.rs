//! The Search Editor's text, in the standard format (`searchEditorSerialization.ts`): search
//! results as an editable document, and the `# Query:` header a saved `.code-search` file
//! starts with.
//!
//! ```text
//! 3 results - 2 files
//!
//! src/lib.rs:
//!   11  fn helper() {
//!   12:     let foo = 1;
//!   13      foo
//!
//! src/main.rs:
//!   4:     foo();
//! ```

use std::path::Path;

use text::Pos;

/// What a search editor searches for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub include: String,
    pub exclude: String,
    /// False: `IgnoreExcludeSettings` (search ignored files too).
    pub use_ignore_files: bool,
    pub context_lines: usize,
}

impl Config {
    pub fn new() -> Self {
        Config { use_ignore_files: true, ..Default::default() }
    }

    /// The `# Query:` lines a saved search editor starts with, ending with a blank line.
    pub fn header(&self) -> String {
        let escape = |s: &str| s.replace('\\', "\\\\").replace('\n', "\\n");
        let mut lines = vec![format!("# Query: {}", escape(&self.query))];
        if self.case_sensitive || self.whole_word || self.regex || !self.use_ignore_files {
            let mut flags = Vec::new();
            if self.case_sensitive {
                flags.push("CaseSensitive");
            }
            if self.whole_word {
                flags.push("WordMatch");
            }
            if self.regex {
                flags.push("RegExp");
            }
            if !self.use_ignore_files {
                flags.push("IgnoreExcludeSettings");
            }
            lines.push(format!("# Flags: {}", flags.join(" ")));
        }
        if !self.include.is_empty() {
            lines.push(format!("# Including: {}", self.include));
        }
        if !self.exclude.is_empty() {
            lines.push(format!("# Excluding: {}", self.exclude));
        }
        if self.context_lines > 0 {
            lines.push(format!("# ContextLines: {}", self.context_lines));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    pub fn search_query(&self) -> search::Query {
        search::Query {
            pattern: self.query.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            use_ignore_files: self.use_ignore_files,
        }
    }
}

/// A saved search editor: its configuration (from the header lines, up to the first blank
/// one) and the results after it.
pub fn parse(text: &str) -> (Config, String) {
    let mut config = Config::new();
    let mut lines = text.split('\n');
    for line in lines.by_ref() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.strip_prefix("# ").and_then(|l| l.split_once(": ")) else { continue };
        match key {
            "Query" => config.query = unescape(value),
            "Including" => config.include = value.to_string(),
            "Excluding" => config.exclude = value.to_string(),
            "ContextLines" => config.context_lines = value.trim().parse().unwrap_or(0),
            "Flags" => {
                config.regex = value.contains("RegExp");
                config.case_sensitive = value.contains("CaseSensitive");
                config.whole_word = value.contains("WordMatch");
                config.use_ignore_files = !value.contains("IgnoreExcludeSettings");
            }
            _ => {}
        }
    }
    (config, lines.collect::<Vec<_>>().join("\n"))
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One file's results: its label (the path relative to the folder), its lines, and the
/// matches in it (sorted).
pub struct FileResults<'a> {
    pub label: String,
    pub lines: Vec<&'a str>,
    pub matches: &'a [search::Match],
}

/// The results as the Search Editor's text, and where the matches are in it.
pub fn serialize(files: &[FileResults], context: usize, limit_hit: bool) -> (String, Vec<(Pos, Pos)>) {
    let count: usize = files.iter().map(|f| f.matches.len()).sum();
    let mut text: Vec<String> = Vec::new();
    text.push(if count == 0 {
        "No Results".to_string()
    } else {
        let results = if count > 1 { format!("{count} results") } else { "1 result".to_string() };
        let files = if files.len() > 1 { format!("{} files", files.len()) } else { "1 file".to_string() };
        format!("{results} - {files}")
    });
    if limit_hit {
        text.push("The result set only contains a subset of all matches. Be more specific in your search to narrow down the results.".into());
    }
    text.push(String::new());
    let mut ranges = Vec::new();
    for f in files {
        if f.matches.is_empty() {
            continue;
        }
        file_block(f, context, &mut text, &mut ranges);
        text.push(String::new());
    }
    (text.join("\n"), ranges)
}

fn file_block(f: &FileResults, context: usize, text: &mut Vec<String>, ranges: &mut Vec<(Pos, Pos)>) {
    let longest = (f.matches.last().map_or(0, |m| m.line) + 1).to_string().len();
    text.push(format!("{}:", f.label));
    let match_lines: std::collections::BTreeSet<usize> = f.matches.iter().map(|m| m.line).collect();
    // Context lines: those around matches that aren't matches themselves.
    let mut ctx: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for &l in &match_lines {
        for c in l.saturating_sub(context)..=(l + context).min(f.lines.len().saturating_sub(1)) {
            if !match_lines.contains(&c) {
                ctx.insert(c);
            }
        }
    }
    let mut ctx: std::collections::VecDeque<usize> = ctx.into_iter().collect();
    let line_text = |l: usize| f.lines.get(l).copied().unwrap_or("").trim_end_matches('\r');
    let mut last: Option<usize> = None;
    let mut line_of = std::collections::HashMap::new();
    for m in f.matches {
        if !line_of.contains_key(&m.line) {
            while ctx.front().is_some_and(|&c| c < m.line) {
                let c = ctx.pop_front().unwrap();
                if last.is_some_and(|l| c != l + 1) {
                    text.push(String::new());
                }
                let n = (c + 1).to_string();
                text.push(format!("  {}{n}  {}", " ".repeat(longest.saturating_sub(n.len())), line_text(c)));
                last = Some(c);
            }
            let n = (m.line + 1).to_string();
            line_of.insert(m.line, (text.len(), 2 + longest.max(n.len()) + 2));
            text.push(format!("  {}{n}: {}", " ".repeat(longest.saturating_sub(n.len())), line_text(m.line)));
            last = Some(m.line);
        }
        let (row, prefix) = line_of[&m.line];
        let src = line_text(m.line);
        let col = |byte: usize| src[..byte.min(src.len())].chars().count();
        ranges.push((Pos::new(row, prefix + col(m.start)), Pos::new(row, prefix + col(m.end))));
    }
    // Context after the last match (doesn't pad these).
    for c in ctx {
        text.push(format!("  {}  {}", c + 1, line_text(c)));
    }
}

/// Where a line of results points: the file's label, and the 0-based line and column for
/// `col` on that line (None on headers, paths and blank lines).
pub fn location(lines: &[&str], line: usize, col: usize) -> Option<(String, usize, usize)> {
    let text = lines.get(line)?;
    let rest = text.strip_prefix("  ")?;
    let digits_at = 2 + (rest.len() - rest.trim_start().len());
    let digits: String = text[digits_at..].chars().take_while(char::is_ascii_digit).collect();
    let n: usize = digits.parse().ok()?;
    let sep = text[digits_at + digits.len()..].chars().next()?;
    if sep != ':' && sep != ' ' {
        return None;
    }
    let prefix = text[..digits_at + digits.len() + 2.min(text.len() - digits_at - digits.len())].chars().count();
    let label = lines[..line].iter().rev().find(|l| !l.is_empty() && !l.starts_with(char::is_whitespace) && l.ends_with(':'))?;
    Some((label[..label.len() - 1].to_string(), n.saturating_sub(1), col.saturating_sub(prefix)))
}

/// The matches of `query` in results text (a saved search, whose match positions aren't
/// saved): on result lines, in the code after the line number.
pub fn find_matches(text: &str, query: &search::Query) -> Vec<(Pos, Pos)> {
    let Ok(regex) = query.compile() else { return Vec::new() };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        let digits_at = 2 + (rest.len() - rest.trim_start().len());
        let digits = line[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !line[digits_at + digits..].starts_with(": ") {
            continue;
        }
        let code_at = digits_at + digits + 2;
        let code = &line[code_at..];
        let prefix = line[..code_at].chars().count();
        for m in search::search_text(code, &regex) {
            let col = |b: usize| prefix + code[..b.min(code.len())].chars().count();
            out.push((Pos::new(i, col(m.start)), Pos::new(i, col(m.end))));
        }
    }
    out
}

/// What a document shows of file text `disk`: a saved search's results without its header.
pub fn document_text(lang: language::Lang, disk: String) -> String {
    if lang == language::Lang::SearchResult {
        parse(&disk).1
    } else {
        disk
    }
}

/// A tab title for a search: "Search: foo".
pub fn title(query: &str) -> String {
    let q: String = query.lines().next().unwrap_or("").chars().take(40).collect();
    if q.is_empty() {
        "Search".into()
    } else {
        format!("Search: {q}")
    }
}

/// Whether `path` is a saved search editor.
pub fn is_search_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "code-search")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(line: usize, start: usize, end: usize) -> search::Match {
        search::Match { line, start, end, preview: String::new(), preview_start: 0, preview_end: 0 }
    }

    #[test]
    fn serializes_results() {
        let a = "fn helper() {\n    let foo = 1;\n    foo\n}\n\n\n\n\n\nfoo";
        let lines: Vec<&str> = a.lines().collect();
        let matches = [m(1, 8, 11), m(2, 4, 7), m(9, 0, 3)];
        let files = [FileResults { label: "src/lib.rs".into(), lines, matches: &matches }];
        let (text, ranges) = serialize(&files, 1, false);
        assert_eq!(
            text,
            "3 results - 1 file\n\nsrc/lib.rs:\n   1  fn helper() {\n   2:     let foo = 1;\n   3:     foo\n   4  }\n\n   9  \n  10: foo\n"
        );
        // Line 4 of the text is `   2:     let foo = 1;` ("  " + padding + "2: " is 6 chars).
        assert_eq!(ranges[0], (Pos::new(4, 6 + 8), Pos::new(4, 6 + 11)));
        assert_eq!(ranges[2], (Pos::new(9, 6), Pos::new(9, 9)));

        let t: Vec<&str> = text.lines().collect();
        assert_eq!(location(&t, 4, 14), Some(("src/lib.rs".into(), 1, 8)));
        assert_eq!(location(&t, 6, 0), Some(("src/lib.rs".into(), 3, 0)));
        assert_eq!(location(&t, 2, 3), None);
        assert_eq!(serialize(&[], 0, false).0, "No Results\n");
    }

    #[test]
    fn header_round_trips() {
        let mut c = Config::new();
        c.query = "a\\b\nc".into();
        c.regex = true;
        c.case_sensitive = true;
        c.include = "src".into();
        c.context_lines = 2;
        let h = c.header();
        assert_eq!(h, "# Query: a\\\\b\\nc\n# Flags: CaseSensitive RegExp\n# Including: src\n# ContextLines: 2\n");
        let (back, body) = parse(&format!("{h}\n1 result - 1 file\n"));
        assert_eq!(back, c);
        assert_eq!(body, "1 result - 1 file\n");
        assert_eq!(title("foo"), "Search: foo");
        let q = search::Query { pattern: "foo".into(), ..Default::default() };
        assert_eq!(find_matches("1 result - 1 file\n\na.rs:\n  3: let foo = 1;\n  4  foo", &q), vec![(Pos::new(3, 9), Pos::new(3, 12))]);
    }
}
