//! `when` clauses (`view == todos && viewItem != done`): context keys compared with
//! `==`, `!=` and `=~` (a regex, matched as a plain substring unless it's `/^...$/`), combined
//! with `!`, `&&`, `||` and parentheses. A bare key is true when it's set and not "false", "0"
//! or empty. `config.<setting>` keys read settings. Keys the editor doesn't know are unset.

use std::collections::HashMap;

/// The context keys and their values.
pub type Context<'a> = HashMap<&'a str, String>;

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Op(&'static str),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let op = match (c, next) {
            ('&', Some('&')) => Some("&&"),
            ('|', Some('|')) => Some("||"),
            ('=', Some('=')) => Some("=="),
            ('!', Some('=')) => Some("!="),
            ('=', Some('~')) => Some("=~"),
            _ => None,
        };
        if let Some(op) = op {
            // `===` and `!==` mean the same.
            i += 2;
            if chars.get(i) == Some(&'=') {
                i += 1;
            }
            out.push(Tok::Op(op));
            continue;
        }
        match c {
            ' ' | '\t' => i += 1,
            '!' | '(' | ')' => {
                out.push(Tok::Op(match c {
                    '!' => "!",
                    '(' => "(",
                    _ => ")",
                }));
                i += 1;
            }
            '\'' | '"' => {
                let end = chars[i + 1..].iter().position(|&ch| ch == c).map_or(chars.len(), |p| i + 1 + p);
                out.push(Tok::Str(chars[i + 1..end].iter().collect()));
                i = end + 1;
            }
            _ => {
                let start = i;
                while i < chars.len() && !matches!(chars[i], ' ' | '\t' | '!' | '(' | ')' | '&' | '|' | '=') {
                    i += 1;
                }
                out.push(Tok::Word(chars[start..i].iter().collect()));
            }
        }
    }
    out
}

struct Parser<'a, 'c> {
    toks: Vec<Tok>,
    i: usize,
    ctx: &'a Context<'c>,
    /// Keys the context doesn't have (`config.*`).
    fallback: &'a dyn Fn(&str) -> Option<String>,
}

impl Parser<'_, '_> {
    fn peek_op(&self, op: &str) -> bool {
        matches!(self.toks.get(self.i), Some(Tok::Op(o)) if *o == op)
    }

    fn or(&mut self) -> bool {
        let mut v = self.and();
        while self.peek_op("||") {
            self.i += 1;
            let r = self.and();
            v = v || r;
        }
        v
    }

    fn and(&mut self) -> bool {
        let mut v = self.unary();
        while self.peek_op("&&") {
            self.i += 1;
            let r = self.unary();
            v = v && r;
        }
        v
    }

    fn unary(&mut self) -> bool {
        if self.peek_op("!") {
            self.i += 1;
            return !self.unary();
        }
        if self.peek_op("(") {
            self.i += 1;
            let v = self.or();
            if self.peek_op(")") {
                self.i += 1;
            }
            return v;
        }
        let key = match self.toks.get(self.i) {
            Some(Tok::Word(w) | Tok::Str(w)) => w.clone(),
            _ => {
                self.i += 1;
                return false;
            }
        };
        self.i += 1;
        let value = self.ctx.get(key.as_str()).cloned().or_else(|| (self.fallback)(&key));
        for op in ["==", "!=", "=~"] {
            if self.peek_op(op) {
                self.i += 1;
                let rhs = match self.toks.get(self.i) {
                    Some(Tok::Word(w) | Tok::Str(w)) => w.clone(),
                    _ => String::new(),
                };
                self.i += 1;
                let actual = value.unwrap_or_default();
                return match op {
                    "==" => actual == rhs,
                    "!=" => actual != rhs,
                    _ => regex_like(&actual, &rhs),
                };
            }
        }
        match key.as_str() {
            "true" => true,
            "false" => false,
            _ => value.is_some_and(|v| !v.is_empty() && v != "false" && v != "0"),
        }
    }
}

/// `=~ /pattern/flags`: anchors and plain text (most `when` regexes are `/^name$/` or a word).
fn regex_like(value: &str, pattern: &str) -> bool {
    let p = pattern.trim_start_matches('/');
    let p = p.rsplit_once('/').map_or(p, |(body, _flags)| body);
    let (start, p) = p.strip_prefix('^').map_or((false, p), |r| (true, r));
    let (end, p) = p.strip_suffix('$').map_or((false, p), |r| (true, r));
    let p = p.replace('\\', "");
    match (start, end) {
        (true, true) => value == p,
        (true, false) => value.starts_with(&p),
        (false, true) => value.ends_with(&p),
        _ => value.contains(&p),
    }
}

/// Evaluates `clause`; an empty or missing clause is true.
#[cfg(test)]
pub fn eval(clause: Option<&str>, ctx: &Context) -> bool {
    eval_with(clause, ctx, &|_| None)
}

/// Evaluates `clause` (an empty or missing clause is true), asking `fallback` for keys `ctx`
/// doesn't have.
pub fn eval_with(clause: Option<&str>, ctx: &Context, fallback: &dyn Fn(&str) -> Option<String>) -> bool {
    let Some(clause) = clause.filter(|c| !c.trim().is_empty()) else { return true };
    Parser { toks: tokenize(clause), i: 0, ctx, fallback }.or()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_clauses() {
        let ctx: Context = [("view", "todos".to_string()), ("viewItem", "file".to_string()), ("flag", "true".to_string())].into();
        assert!(eval(None, &ctx));
        assert!(eval(Some("view == todos"), &ctx));
        assert!(eval(Some("view == 'todos' && viewItem != dir"), &ctx));
        assert!(!eval(Some("view == other || viewItem == dir"), &ctx));
        assert!(eval(Some("!unknownKey && flag"), &ctx));
        assert!(eval(Some("view == x || (flag && viewItem =~ /^fi/)"), &ctx));
        assert!(!eval(Some("viewItem =~ /^dir$/"), &ctx));
        assert!(eval(Some("view === todos"), &ctx));
        let config = |k: &str| (k == "config.a.b").then(|| "true".to_string());
        assert!(eval_with(Some("config.a.b && view == todos"), &ctx, &config));
        assert!(!eval_with(Some("config.a.c"), &ctx, &config));
    }
}
