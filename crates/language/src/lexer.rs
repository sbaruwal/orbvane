//! Line-by-line lexical highlighter, used for languages without a tree-sitter grammar
//! (Markdown, plain text) and as a fallback if parsing fails.

use theme::Token;

use crate::lang::Lang;

/// Lexer state carried from the end of one line to the start of the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineState {
    #[default]
    Normal,
    BlockComment,
    TripleString(u8),
    /// In a Search Editor's results for a file in this language.
    SearchBlock(Lang),
}

pub type Span = (usize, usize, Token);

/// Tokenizes one line, returning colored byte ranges and the state at the end of the line.
pub fn highlight_line(lang: Lang, line: &str, state: LineState, out: &mut Vec<Span>) -> LineState {
    let def = lang.def();
    if lang == Lang::Markdown {
        return markdown_line(line, out);
    }
    if lang == Lang::PlainText {
        return LineState::Normal;
    }
    if lang == Lang::SearchResult {
        return search_result_line(line, state, out);
    }
    let b = line.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut state = state;
    let is_json = lang == Lang::Json;
    let is_toml = lang == Lang::Toml;

    while i < n {
        match state {
            LineState::BlockComment => {
                let end = def.block_comment.map(|(_, e)| e).unwrap_or("*/");
                match line[i..].find(end) {
                    Some(p) => {
                        out.push((i, i + p + end.len(), Token::Comment));
                        i += p + end.len();
                        state = LineState::Normal;
                    }
                    None => {
                        out.push((i, n, Token::Comment));
                        return state;
                    }
                }
                continue;
            }
            LineState::TripleString(q) => {
                let delim = [q, q, q];
                let delim = std::str::from_utf8(&delim).unwrap();
                match line[i..].find(delim) {
                    Some(p) => {
                        out.push((i, i + p + 3, Token::String));
                        i += p + 3;
                        state = LineState::Normal;
                    }
                    None => {
                        out.push((i, n, Token::String));
                        return state;
                    }
                }
                continue;
            }
            LineState::Normal | LineState::SearchBlock(_) => {}
        }

        let c = b[i];
        let rest = &line[i..];
        // Block comments first: Lua's `--[[` starts with its line comment `--`.
        if let Some((start, _)) = def.block_comment {
            if rest.starts_with(start) {
                out.push((i, i + start.len(), Token::Comment));
                i += start.len();
                state = LineState::BlockComment;
                continue;
            }
        }
        if let Some(lc) = def.line_comment {
            if rest.starts_with(lc) {
                out.push((i, n, Token::Comment));
                return state;
            }
        }
        if def.triple_strings && (rest.starts_with("\"\"\"") || rest.starts_with("'''")) {
            out.push((i, i + 3, Token::String));
            i += 3;
            state = LineState::TripleString(c);
            continue;
        }
        if lang == Lang::Rust && (rest.starts_with("#[") || rest.starts_with("#![")) {
            let end = rest.find(']').map_or(n, |p| i + p + 1);
            out.push((i, end, Token::Attribute));
            i = end;
            continue;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            // Rust lifetimes ('a) look like char literals; only treat 'x' as a char if it closes.
            if c == b'\'' && def.lifetimes {
                let close = rest[1..].char_indices().nth(1).map(|(p, _)| p + 1);
                let closes = close.is_some_and(|p| rest.as_bytes().get(p) == Some(&b'\''))
                    || rest.starts_with("'\\");
                if !closes {
                    let end = i + 1 + ident_len(&b[i + 1..]);
                    out.push((i, end, Token::Lifetime));
                    i = end.max(i + 1);
                    continue;
                }
            }
            let end = string_end(b, i, c);
            // In JSON and TOML, a string followed by ':' or '=' is a key.
            let after = line[end..].trim_start();
            let token = if (is_json && after.starts_with(':')) || (is_toml && after.starts_with('=')) {
                Token::Variable
            } else {
                Token::String
            };
            out.push((i, end, token));
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let mut end = i + 1;
            while end < n && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'.') {
                if b[end] == b'.' && !b.get(end + 1).is_some_and(|d| d.is_ascii_digit()) {
                    break;
                }
                end += 1;
            }
            out.push((i, end, Token::Number));
            i = end;
            continue;
        }
        if is_ident_start(c) || (c == b'#' && !def.hash_comment) {
            let end = i + 1 + ident_len(&b[i + 1..]);
            let word = &line[i..end];
            let next = b.get(end).copied();
            let token = if def.control.contains(&word) {
                Some(Token::ControlKeyword)
            } else if def.keywords.contains(&word) {
                Some(Token::Keyword)
            } else if def.constants.contains(&word) {
                Some(Token::Constant)
            } else if is_toml && i == line.len() - line.trim_start().len() && line[end..].trim_start().starts_with('=')
            {
                Some(Token::Variable)
            } else if def.macros && next == Some(b'!') {
                Some(Token::Macro)
            } else if next == Some(b'(') {
                Some(Token::Function)
            } else if word.len() > 1 && word.bytes().all(|c| c.is_ascii_uppercase() || c == b'_' || c.is_ascii_digit())
            {
                Some(Token::Constant)
            } else if c.is_ascii_uppercase() {
                Some(Token::Type)
            } else if is_toml || is_json {
                None
            } else {
                Some(Token::Variable)
            };
            if let Some(t) = token {
                let end = if t == Token::Macro { end + 1 } else { end };
                out.push((i, end, t));
                i = end;
            } else {
                i = end;
            }
            continue;
        }
        if is_toml && c == b'[' && line.trim_start().starts_with('[') && i == line.len() - line.trim_start().len() {
            out.push((i, n, Token::Type));
            return state;
        }
        // Skip one (possibly multi-byte) char.
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    state
}

/// A line of Search Editor results, like `search-result` grammar: `# Key: value`
/// headers, file paths ("src/lib.rs:") as strings, and result lines ("  12: code", context
/// "  13  code") with numbered prefixes and the code highlighted in its file's language.
fn search_result_line(line: &str, state: LineState, out: &mut Vec<Span>) -> LineState {
    let in_file = match state {
        LineState::SearchBlock(lang) => lang,
        _ => Lang::PlainText,
    };
    if let Some(rest) = line.strip_prefix("# ") {
        if let Some(colon) = rest.find(": ") {
            out.push((0, 2 + colon + 1, Token::Attribute));
            out.push((2 + colon + 2, line.len(), Token::String));
        }
        return state;
    }
    if !line.is_empty() && !line.starts_with(char::is_whitespace) && line.ends_with(':') {
        out.push((0, line.len() - 1, Token::String));
        let path = std::path::Path::new(&line[..line.len() - 1]);
        return LineState::SearchBlock(Lang::detect(Some(path)));
    }
    if let Some(rest) = line.strip_prefix("  ") {
        let digits_at = 2 + (rest.len() - rest.trim_start().len());
        let digits = line[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
        let sep = line.as_bytes().get(digits_at + digits).copied();
        if digits > 0 && matches!(sep, Some(b':') | Some(b' ')) {
            out.push((digits_at, digits_at + digits, Token::Number));
            let code_at = (digits_at + digits + 2).min(line.len());
            if in_file != Lang::PlainText && in_file != Lang::SearchResult {
                let mut code = Vec::new();
                highlight_line(in_file, &line[code_at..], LineState::Normal, &mut code);
                out.extend(code.into_iter().map(|(a, b, t)| (a + code_at, b + code_at, t)));
            }
        }
    }
    state
}

fn markdown_line(line: &str, out: &mut Vec<Span>) -> LineState {
    let t = line.trim_start();
    let indent = line.len() - t.len();
    if t.starts_with('#') {
        out.push((indent, line.len(), Token::Keyword));
    } else if t.starts_with("```") {
        out.push((indent, line.len(), Token::String));
    } else if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("> ") {
        out.push((indent, indent + 1, Token::Keyword));
    }
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            if let Some(p) = line[i + 1..].find('`') {
                out.push((i, i + p + 2, Token::String));
                i += p + 2;
                continue;
            }
        }
        i += 1;
    }
    LineState::Normal
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn ident_len(b: &[u8]) -> usize {
    b.iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count()
}

fn string_end(b: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Line-start lexer states, computed lazily up to the lines asked for and recomputed from the
/// first changed line, so highlighting cost follows what's on screen, not the document's size.
#[derive(Default)]
pub struct HighlightCache {
    /// States for lines `0..states.len()` (the state each line starts in).
    states: Vec<LineState>,
}

impl HighlightCache {
    /// Forgets the states from the first line `changes` touch.
    pub fn invalidate(&mut self, changes: &[text::Change]) {
        for change in changes {
            match change {
                text::Change::Edit(e) => self.states.truncate(e.start.0 + 1),
                text::Change::Reset => self.states.clear(),
            }
        }
    }

    /// The state line `line` starts in (computing the lines before it as needed).
    pub fn state(&mut self, lang: Lang, buffer: &text::Buffer, line: usize) -> LineState {
        let needs_state = lang.def().block_comment.is_some() || lang.def().triple_strings || lang == Lang::SearchResult;
        if !needs_state {
            return LineState::Normal;
        }
        let line = line.min(buffer.len_lines());
        if self.states.is_empty() {
            self.states.push(LineState::Normal);
        }
        let mut scratch = Vec::new();
        if self.states.len() <= line {
            let from = self.states.len() - 1;
            for text in buffer.lines_from(from).take(line - from) {
                scratch.clear();
                let next = highlight_line(lang, &text, *self.states.last().unwrap(), &mut scratch);
                self.states.push(next);
            }
        }
        self.states.get(line).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_results() {
        let mut out = Vec::new();
        let st = highlight_line(Lang::SearchResult, "src/lib.rs:", LineState::Normal, &mut out);
        assert_eq!(st, LineState::SearchBlock(Lang::Rust));
        assert_eq!(out, vec![(0, 10, Token::String)]);
        out.clear();
        let line = "  12: let x = 1;";
        highlight_line(Lang::SearchResult, line, st, &mut out);
        assert_eq!(out[0], (2, 4, Token::Number));
        assert!(out.iter().any(|&(a, b, t)| &line[a..b] == "let" && t == Token::Keyword), "{out:?}");
        out.clear();
        highlight_line(Lang::SearchResult, "# Query: foo", LineState::Normal, &mut out);
        assert_eq!(out, vec![(0, 8, Token::Attribute), (9, 12, Token::String)]);
    }

    fn tokens(lang: Lang, line: &str) -> Vec<(String, Token)> {
        let mut out = Vec::new();
        highlight_line(lang, line, LineState::Normal, &mut out);
        out.into_iter().map(|(a, b, t)| (line[a..b].to_string(), t)).collect()
    }

    #[test]
    fn rust_tokens() {
        let t = tokens(Lang::Rust, "pub fn main() { let x: Vec<u8> = vec![1]; // hi");
        assert!(t.contains(&("pub".into(), Token::Keyword)));
        assert!(t.contains(&("main".into(), Token::Function)));
        assert!(t.contains(&("Vec".into(), Token::Type)));
        assert!(t.contains(&("vec!".into(), Token::Macro)));
        assert!(t.contains(&("1".into(), Token::Number)));
        assert!(t.contains(&("// hi".into(), Token::Comment)));
    }

    #[test]
    fn rust_lifetime_vs_char() {
        let t = tokens(Lang::Rust, "fn f<'a>(c: char) { 'x' }");
        assert!(t.contains(&("'a".into(), Token::Lifetime)));
        assert!(t.contains(&("'x'".into(), Token::String)));
    }

    #[test]
    fn block_comment_spans_lines() {
        let mut out = Vec::new();
        let s = highlight_line(Lang::Rust, "let a = 1; /* start", LineState::Normal, &mut out);
        assert_eq!(s, LineState::BlockComment);
        out.clear();
        let s = highlight_line(Lang::Rust, "end */ let", s, &mut out);
        assert_eq!(s, LineState::Normal);
        assert_eq!(out[0], (0, 6, Token::Comment));
    }
}
