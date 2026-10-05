//! Format Document for CSS, SCSS and Less with the defaults (`css.format.*`): one
//! declaration per line, each selector of a list on its own line, a blank line between rules,
//! `prop: value`, and blank lines kept where there were some. Only whitespace between tokens
//! changes.

use std::collections::HashSet;

use crate::parse::{Kind, Stylesheet, T};

pub fn format(s: &Stylesheet, tab: &str, eol: &str) -> Vec<((usize, usize), String)> {
    let text = &s.text;
    let toks: Vec<_> = s.tokens.iter().filter(|t| !matches!(t.kind, T::Whitespace | T::Eof)).copied().collect();
    let colons: HashSet<usize> = s.nodes.iter().filter_map(|n| n.colon).collect();
    let rule_starts: HashSet<usize> =
        s.nodes.iter().filter(|n| n.kind == Kind::RuleSet || (n.kind == Kind::AtRule && n.block.is_some())).map(|n| n.start).collect();
    // Commas between the selectors of a rule (not inside `:is(a, b)`).
    let mut selector_commas = HashSet::new();
    for n in s.nodes.iter().filter(|n| n.kind == Kind::RuleSet) {
        let mut depth = 0;
        for t in s.tokens_in(n.name) {
            match t.kind {
                T::ParenL | T::Function | T::BracketL => depth += 1,
                T::ParenR | T::BracketR => depth -= 1,
                T::Comma if depth == 0 => {
                    selector_commas.insert(t.start);
                }
                _ => {}
            }
        }
    }
    let newline = |depth: usize, blank: bool| format!("{}{eol}{}", if blank { eol } else { "" }, tab.repeat(depth));
    let line_comment = |t: &crate::parse::Token| t.kind == T::Comment && text[t.start..].starts_with("//");
    let mut edits = Vec::new();
    let mut depth = 0usize;
    for i in 0..toks.len() {
        let t = toks[i];
        if t.kind == T::CurlyR {
            depth = depth.saturating_sub(1);
        }
        if let Some(&p) = i.checked_sub(1).map(|j| &toks[j]) {
            let gap = &text[p.end..t.start];
            let lines = gap.matches('\n').count();
            let keep_blank = lines >= 2;
            let want = if t.kind == T::Comment {
                if lines > 0 { newline(depth, keep_blank) } else { " ".into() }
            } else if line_comment(&p) || (p.kind == T::Comment && lines > 0) {
                newline(depth, keep_blank)
            } else if t.kind == T::CurlyR {
                if p.kind == T::CurlyL { String::new() } else { newline(depth, false) }
            } else if p.kind == T::CurlyL {
                newline(depth, false)
            } else if p.kind == T::CurlyR {
                if t.kind == T::Semicolon { String::new() } else { newline(depth, keep_blank || rule_starts.contains(&t.start)) }
            } else if p.kind == T::Semicolon {
                newline(depth, keep_blank || rule_starts.contains(&t.start))
            } else if t.kind == T::CurlyL {
                " ".into()
            } else if p.kind == T::Comma && selector_commas.contains(&p.start) {
                newline(depth, false)
            } else if matches!(t.kind, T::Comma | T::Semicolon) || colons.contains(&t.start) {
                String::new()
            } else if colons.contains(&p.start) || p.kind == T::Comma {
                " ".into()
            } else if gap.is_empty() {
                String::new()
            } else if lines > 0 {
                // A value continued on the next line (grid areas) stays there.
                newline(depth + 1, false)
            } else {
                " ".into()
            };
            if gap != want {
                edits.push(((p.end, t.start), want));
            }
        }
        if t.kind == T::CurlyL {
            depth += 1;
        }
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Syntax;

    fn formatted(text: &str) -> String {
        let s = Stylesheet::parse(text, Syntax::Scss);
        let mut out = text.to_string();
        for ((a, b), new) in format(&s, "    ", "\n").into_iter().rev() {
            out.replace_range(a..b, &new);
        }
        out
    }

    #[test]
    fn formats_like_the_reference() {
        assert_eq!(
            formatted("a,b{color:red;margin:0 auto}\n.c{}\n@media screen{d{e:rgba(0,0,0,.5)}}"),
            "a,\nb {\n    color: red;\n    margin: 0 auto\n}\n\n.c {}\n\n@media screen {\n    d {\n        e: rgba(0, 0, 0, .5)\n    }\n}"
        );
        // Comments and blank lines between declarations stay.
        assert_eq!(formatted("a {\n  color: red; // why\n\n  b: c;\n}"), "a {\n    color: red; // why\n\n    b: c;\n}");
        // Pseudo-classes and functions keep their spacing.
        assert_eq!(formatted("a:hover > b:not(.x) { c: d }"), "a:hover > b:not(.x) {\n    c: d\n}");
    }
}
