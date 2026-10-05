//! Diagnostics: syntax errors and the default lint rules
//! (`css.lint.*`), with its messages.

use crate::data::{data, LESS_AT_RULES, SCSS_AT_RULES};
use crate::parse::{Kind, Stylesheet, Syntax, T};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub severity: Severity,
}

pub fn lint(s: &Stylesheet) -> Vec<Problem> {
    let mut out: Vec<Problem> = s.errors.iter().map(|e| Problem { start: e.start, end: e.end, message: e.message.to_string(), severity: Severity::Error }).collect();
    let warn = |out: &mut Vec<Problem>, (start, end): (usize, usize), message: String| out.push(Problem { start, end, message, severity: Severity::Warning });
    for (id, n) in s.nodes.iter().enumerate() {
        match n.kind {
            Kind::RuleSet => {
                if matches!(n.block, Some((_, Some(_)))) && n.children.is_empty() {
                    warn(&mut out, n.name, "Do not use empty rulesets".into());
                }
                block_rules(s, id, &mut out);
            }
            Kind::AtRule => {
                let name = s.slice(n.name);
                let lower = name.to_ascii_lowercase();
                let known = data().at_directive(&lower).is_some()
                    || lower.starts_with("@-")
                    || match s.syntax {
                        Syntax::Scss => SCSS_AT_RULES.iter().any(|(r, _)| *r == lower),
                        Syntax::Less => LESS_AT_RULES.iter().any(|(r, _)| *r == lower),
                        Syntax::Css => false,
                    };
                if !known {
                    warn(&mut out, n.name, format!("Unknown at rule {name}"));
                }
                if lower == "@font-face" && n.block.is_some() {
                    let has = |p: &str| n.children.iter().any(|&c| s.node(c).kind == Kind::Declaration && s.slice(s.node(c).name).eq_ignore_ascii_case(p));
                    if !has("src") || !has("font-family") {
                        warn(&mut out, n.name, "@font-face rule must define 'src' and 'font-family' properties".into());
                    }
                }
                if n.block.is_some() && s.holds_declarations(id) {
                    block_rules(s, id, &mut out);
                }
            }
            Kind::Declaration => {
                values(s, n.value, s.slice(n.name), &mut out);
            }
            Kind::Variable => values(s, n.value, "", &mut out),
            Kind::Stylesheet => {
                if n.block.is_some() {
                    block_rules(s, id, &mut out);
                }
            }
        }
    }
    out.sort_by_key(|p| p.start);
    out
}

/// Whether a property name can be checked (no hacks, interpolation or custom names).
fn checkable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("--")
        && !name.contains(['#', '@', '$', '{', '*', '_'])
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The rules about one block's declarations: unknown properties, vendor prefixes, and
/// properties the display value ignores.
fn block_rules(s: &Stylesheet, block: usize, out: &mut Vec<Problem>) {
    let decls: Vec<(String, (usize, usize), String)> = s
        .node(block)
        .children
        .iter()
        .map(|&c| s.node(c))
        .filter(|d| d.kind == Kind::Declaration && d.block.is_none())
        .map(|d| (s.slice(d.name).to_ascii_lowercase(), d.name, s.slice(d.value).trim().to_ascii_lowercase()))
        .collect();
    let has = |p: &str| decls.iter().any(|(n, _, _)| n == p);
    let warn = |out: &mut Vec<Problem>, (start, end): (usize, usize), message: &str| {
        out.push(Problem { start, end, message: message.to_string(), severity: Severity::Warning })
    };
    for (name, range, _) in &decls {
        if !checkable(name) {
            continue;
        }
        if let Some(rest) = name.strip_prefix('-') {
            // `-webkit-transition`: the standard `transition` should be there too.
            let standard = rest.split_once('-').map_or("", |(_, p)| p);
            if data().property(standard).is_some() && !has(standard) {
                warn(out, *range, "When using a vendor-specific prefix also include the standard property");
            }
            continue;
        }
        if data().property(name).is_none() {
            out.push(Problem { start: range.0, end: range.1, message: format!("Unknown property: '{}'", s.slice(*range)), severity: Severity::Warning });
        }
    }
    let display = decls.iter().rev().find(|(n, _, _)| n == "display").map(|(_, r, v)| (*r, v.as_str()));
    let float = decls.iter().rev().find(|(n, _, _)| n == "float").filter(|(_, _, v)| v != "none");
    match display {
        Some((_, "inline")) => {
            for (name, range, _) in &decls {
                if matches!(name.as_str(), "width" | "height" | "margin-top" | "margin-bottom" | "float") {
                    warn(out, *range, "Property is ignored due to the display. With 'display: inline', the width, height, margin-top, margin-bottom, and float properties have no effect.");
                }
            }
        }
        Some((range, "inline-block")) if float.is_some() => {
            warn(out, range, "inline-block is ignored due to the float. If 'float' has a value other than 'none', the box is floated and 'display' is treated as 'block'");
        }
        Some((_, "block")) => {
            for (name, range, _) in &decls {
                if name == "vertical-align" {
                    warn(out, *range, "Property is ignored due to the display. With 'display: block', vertical-align should not be used.");
                }
            }
        }
        _ => {}
    }
}

/// Rules about a value: hex color lengths and color function arguments.
fn values(s: &Stylesheet, range: (usize, usize), _property: &str, out: &mut Vec<Problem>) {
    let toks: Vec<_> = s.tokens_in(range).copied().collect();
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            T::Hash => {
                let hex = &s.text[t.start + 1..t.end];
                if !(matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())) {
                    out.push(Problem { start: t.start, end: t.end, message: "Hex colors must consist of three, four, six or eight hex numbers".into(), severity: Severity::Error });
                }
            }
            T::Function => {
                let name = s.text[t.start..t.end - 1].to_ascii_lowercase();
                if !matches!(name.as_str(), "rgb" | "rgba" | "hsl" | "hsla") {
                    continue;
                }
                // The arguments up to the matching `)`.
                let mut depth = 1;
                let mut args: Vec<Vec<T>> = vec![Vec::new()];
                let mut close = None;
                for (j, a) in toks.iter().enumerate().skip(i + 1) {
                    match a.kind {
                        T::Function | T::ParenL => depth += 1,
                        T::ParenR => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(j);
                                break;
                            }
                        }
                        T::Comma if depth == 1 => args.push(Vec::new()),
                        T::Whitespace | T::Comment => {}
                        k => args.last_mut().unwrap().push(k),
                    }
                }
                let Some(close) = close else { continue };
                let all: Vec<T> = args.iter().flatten().copied().collect();
                // Variables, calc() and relative colors can't be counted.
                if all.iter().any(|k| matches!(k, T::Function | T::Variable | T::AtKeyword | T::Interpolation | T::Ident)) {
                    continue;
                }
                let count = if args.len() > 1 { args.len() } else { all.iter().filter(|k| !matches!(k, T::Delim(b'/'))).count() };
                if !matches!(count, 3 | 4) {
                    out.push(Problem { start: t.start, end: toks[close].end, message: "Invalid number of parameters".into(), severity: Severity::Error });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(text: &str) -> Vec<String> {
        lint(&Stylesheet::parse(text, Syntax::Css)).into_iter().map(|p| format!("{}: {}", &text[p.start..p.end], p.message)).collect()
    }

    #[test]
    fn lints_with_the_default_rules() {
        assert_eq!(messages("a { colr: red; }"), ["colr: Unknown property: 'colr'"]);
        assert_eq!(lint(&Stylesheet::parse_declarations("colr: 1", Syntax::Css)).len(), 1);
        assert_eq!(messages(".x {}"), [".x: Do not use empty rulesets"]);
        assert_eq!(messages("a { -webkit-transition: none; }"), ["-webkit-transition: When using a vendor-specific prefix also include the standard property"]);
        assert!(messages("a { -webkit-transition: none; transition: none; --my-var: 1; }").is_empty());
        assert_eq!(messages("a { color: #12345; }"), ["#12345: Hex colors must consist of three, four, six or eight hex numbers"]);
        assert_eq!(messages("a { color: rgb(1, 2); }"), ["rgb(1, 2): Invalid number of parameters"]);
        assert!(messages("a { color: rgb(1 2 3 / 50%); background: rgb(var(--x)); }").is_empty());
        assert_eq!(messages("@foo x;"), ["@foo: Unknown at rule @foo"]);
        assert_eq!(messages("@font-face { font-family: x; }"), ["@font-face: @font-face rule must define 'src' and 'font-family' properties"]);
        assert_eq!(messages("a { display: inline; width: 1px; }").len(), 1);
    }
}
