//! Stylesheet abbreviations (`m10`, `p10-20`, `db`, `c#f.5`, `pos:a!`) expanded to CSS
//! properties, ported from Emmet (emmetio/emmet, MIT): its tokenizer and parser, fuzzy snippet
//! matching, keyword and unit resolution, and output.

use std::sync::OnceLock;

use super::Out;

#[derive(Clone, Debug, PartialEq)]
enum V {
    Num { value: f64, raw: String, unit: String },
    Color { r: u8, g: u8, b: u8, a: f64 },
    Str { value: String, double: bool },
    Lit(String),
    Field { index: u32, name: String },
    Custom(String),
    Func { name: String, args: Vec<Value> },
}

/// A value token and where it was in its source (fields right after a token don't get a space).
#[derive(Clone, Debug, PartialEq)]
struct T {
    v: V,
    span: Option<(usize, usize)>,
}

type Value = Vec<T>;

#[derive(Clone, Debug)]
enum Tk {
    V(T),
    Ws,
    Op(char),
    Bracket(bool),
}

#[derive(Clone, Debug)]
struct Prop {
    name: Option<String>,
    value: Vec<Value>,
    important: bool,
}

fn t(v: V) -> T {
    T { v, span: None }
}

fn is_alpha_word(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_keyword(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_literal(c: char) -> bool {
    is_alpha_word(c) || c == '%' || c == '/'
}

// ------------------------------------------------------------------ tokenizer

struct Scanner {
    s: Vec<char>,
    pos: usize,
}

impl Scanner {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, f: impl Fn(char) -> bool) -> bool {
        if self.peek().is_some_and(f) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_while(&mut self, f: impl Fn(char) -> bool) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.s[a..b].iter().collect()
    }

    fn token(&mut self, short: bool) -> Result<Option<Tk>, ()> {
        if let Some(f) = self.field()? {
            return Ok(Some(f));
        }
        Ok(self
            .custom_property()
            .or_else(|| self.number())
            .or_else(|| self.color())
            .or_else(|| self.string())
            .or_else(|| self.bracket())
            .or_else(|| self.operator())
            .or_else(|| self.eat_while(|c| c == ' ' || c == '\t' || c == '\u{a0}').then_some(Tk::Ws))
            .or_else(|| self.literal(short)))
    }

    fn field(&mut self) -> Result<Option<Tk>, ()> {
        let start = self.pos;
        if !(self.eat(|c| c == '$') && self.eat(|c| c == '{')) {
            self.pos = start;
            return Ok(None);
        }
        let digits = self.pos;
        let mut index = 0;
        let mut name = String::new();
        if self.eat_while(|c| c.is_ascii_digit()) {
            index = self.slice(digits, self.pos).parse().map_err(|_| ())?;
            if self.eat(|c| c == ':') {
                name = self.placeholder()?;
            }
        } else if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            name = self.placeholder()?;
        }
        if !self.eat(|c| c == '}') {
            return Err(());
        }
        Ok(Some(Tk::V(T { v: V::Field { index, name }, span: Some((start, self.pos)) })))
    }

    fn placeholder(&mut self) -> Result<String, ()> {
        let start = self.pos;
        let mut depth = 0;
        while let Some(c) = self.peek() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => break,
                '}' => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        if depth > 0 {
            return Err(());
        }
        Ok(self.slice(start, self.pos))
    }

    fn custom_property(&mut self) -> Option<Tk> {
        let start = self.pos;
        if self.eat(|c| c == '-') && self.eat(|c| c == '-') {
            self.eat_while(is_keyword);
            return Some(Tk::V(T { v: V::Custom(self.slice(start, self.pos)), span: Some((start, self.pos)) }));
        }
        self.pos = start;
        None
    }

    fn number(&mut self) -> Option<Tk> {
        let start = self.pos;
        self.eat(|c| c == '-');
        let after_minus = self.pos;
        let whole = self.eat_while(|c| c.is_ascii_digit());
        let before_dot = self.pos;
        if self.eat(|c| c == '.') {
            let frac = self.eat_while(|c| c.is_ascii_digit());
            if !whole && !frac {
                self.pos = before_dot;
            }
        }
        if self.pos == after_minus {
            self.pos = start;
            return None;
        }
        let raw = self.slice(start, self.pos);
        let unit_start = self.pos;
        if !self.eat(|c| c == '%') {
            self.eat_while(is_alpha_word);
        }
        let value = raw.parse().ok()?;
        Some(Tk::V(T { v: V::Num { value, raw, unit: self.slice(unit_start, self.pos) }, span: Some((start, self.pos)) }))
    }

    fn color(&mut self) -> Option<Tk> {
        let start = self.pos;
        if !self.eat(|c| c == '#') {
            return None;
        }
        let value_start = self.pos;
        let (color, alpha) = if self.eat_while(|c| c.is_ascii_hexdigit()) {
            let color = self.slice(value_start, self.pos);
            (color, self.alpha())
        } else if self.eat(|c| c == 't') {
            let a = self.alpha();
            ("0".to_string(), if a.is_empty() { "0".into() } else { a })
        } else {
            (String::new(), self.alpha())
        };
        if color.is_empty() && alpha.is_empty() && self.peek().is_some() {
            return Some(Tk::V(T { v: V::Lit("#".into()), span: Some((start, self.pos)) }));
        }
        let hex = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0);
        let (r, g, b) = match color.len() {
            0 => (0, 0, 0),
            1 => {
                let v = hex(&color.repeat(2));
                (v, v, v)
            }
            2 => {
                let v = hex(&color);
                (v, v, v)
            }
            3 => {
                let c: Vec<String> = color.chars().map(|c| c.to_string().repeat(2)).collect();
                (hex(&c[0]), hex(&c[1]), hex(&c[2]))
            }
            _ => {
                let c = color.repeat(2);
                (hex(&c[0..2]), hex(&c[2..4]), hex(&c[4..6]))
            }
        };
        let a = if alpha.is_empty() { 1.0 } else { alpha.parse().unwrap_or(1.0) };
        Some(Tk::V(T { v: V::Color { r, g, b, a }, span: Some((start, self.pos)) }))
    }

    fn alpha(&mut self) -> String {
        let start = self.pos;
        if self.eat(|c| c == '.') {
            if self.eat_while(|c| c.is_ascii_digit()) {
                return self.slice(start, self.pos);
            }
            return "1".into();
        }
        String::new()
    }

    fn string(&mut self) -> Option<Tk> {
        let q = self.peek().filter(|c| *c == '"' || *c == '\'')?;
        let start = self.pos;
        self.pos += 1;
        let mut finished = false;
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == q {
                finished = true;
                break;
            }
        }
        let value = self.slice(start + 1, self.pos - usize::from(finished));
        Some(Tk::V(T { v: V::Str { value, double: q == '"' }, span: Some((start, self.pos)) }))
    }

    fn bracket(&mut self) -> Option<Tk> {
        let c = self.peek().filter(|c| *c == '(' || *c == ')')?;
        self.pos += 1;
        Some(Tk::Bracket(c == '('))
    }

    fn operator(&mut self) -> Option<Tk> {
        let c = self.peek().filter(|c| matches!(c, '+' | '!' | ',' | ':' | '-'))?;
        self.pos += 1;
        Some(Tk::Op(c))
    }

    fn literal(&mut self, short: bool) -> Option<Tk> {
        let start = self.pos;
        if self.eat(|c| c == '@' || c == '$') {
            if start > 0 {
                self.eat_while(is_keyword);
            } else {
                self.eat_while(is_literal);
            }
        } else if self.eat(is_alpha_word) {
            if short {
                self.eat_while(is_literal);
            } else {
                self.eat_while(is_keyword);
            }
        } else {
            self.eat(|c| c == '.');
            self.eat_while(is_literal);
        }
        (self.pos > start).then(|| Tk::V(T { v: V::Lit(self.slice(start, self.pos)), span: Some((start, self.pos)) }))
    }
}

fn tokenize(abbr: &str, is_value: bool) -> Result<Vec<Tk>, ()> {
    let mut sc = Scanner { s: abbr.chars().collect(), pos: 0 };
    let mut brackets = 0i32;
    let mut tokens: Vec<Tk> = Vec::new();
    while sc.peek().is_some() {
        let tok = sc.token(brackets == 0 && !is_value)?.ok_or(())?;
        if let Tk::Bracket(open) = tok {
            if brackets == 0 && open {
                merge_tokens(&sc, &mut tokens);
            }
            brackets += if open { 1 } else { -1 };
            if brackets < 0 {
                return Err(());
            }
        }
        let dash_after = matches!(&tok, Tk::V(T { v: V::Color { .. }, .. }))
            || matches!(&tok, Tk::V(T { v: V::Num { unit, .. }, .. }) if unit.is_empty());
        tokens.push(tok);
        if dash_after {
            if let Some(op) = sc.operator() {
                tokens.push(op);
            }
        }
    }
    Ok(tokens)
}

/// Before a top-level `(`: trailing literal and number tokens become one literal (a function name).
fn merge_tokens(sc: &Scanner, tokens: &mut Vec<Tk>) {
    let (mut start, mut end) = (0, 0);
    while let Some(Tk::V(T { v: V::Lit(_) | V::Num { .. }, span: Some((s, e)) })) = tokens.last() {
        start = *s;
        if end == 0 {
            end = *e;
        }
        tokens.pop();
    }
    if start != end {
        tokens.push(Tk::V(T { v: V::Lit(sc.slice(start, end)), span: Some((start, end)) }));
    }
}

// ------------------------------------------------------------------ parser

struct Parser {
    toks: Vec<Tk>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos)
    }

    fn consume(&mut self, f: impl Fn(&Tk) -> bool) -> bool {
        if self.peek().is_some_and(f) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn property(&mut self, value_mode: bool) -> Result<Option<Prop>, ()> {
        let mut name = None;
        let mut important = false;
        let mut value = Vec::new();
        if !value_mode {
            if let Some(Tk::V(T { v: V::Lit(l), .. })) = self.peek() {
                if !matches!(self.toks.get(self.pos + 1), Some(Tk::Bracket(_))) {
                    name = Some(l.clone());
                    self.pos += 1;
                    self.consume(|t| matches!(t, Tk::Op(':' | '-')));
                }
            }
        }
        if value_mode {
            self.consume(|t| matches!(t, Tk::Ws));
        }
        while self.peek().is_some() {
            if self.consume(|t| matches!(t, Tk::Op('!'))) {
                important = true;
            } else if let Some(v) = self.value(value_mode)? {
                value.push(v);
            } else if !self.consume(|t| matches!(t, Tk::Op(','))) {
                break;
            }
        }
        Ok((name.is_some() || !value.is_empty() || important).then_some(Prop { name, value, important }))
    }

    fn value(&mut self, in_argument: bool) -> Result<Option<Value>, ()> {
        let mut out = Vec::new();
        while let Some(tok) = self.peek().cloned() {
            match tok {
                Tk::V(tv) => {
                    self.pos += 1;
                    if let V::Lit(name) = &tv.v {
                        if let Some(args) = self.arguments()? {
                            out.push(t(V::Func { name: name.clone(), args }));
                            continue;
                        }
                    }
                    out.push(tv);
                }
                Tk::Op(':' | '-') => self.pos += 1,
                Tk::Ws if in_argument => self.pos += 1,
                _ => break,
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }

    fn arguments(&mut self) -> Result<Option<Vec<Value>>, ()> {
        if !self.consume(|t| matches!(t, Tk::Bracket(true))) {
            return Ok(None);
        }
        let mut args = Vec::new();
        while self.peek().is_some() && !self.consume(|t| matches!(t, Tk::Bracket(false))) {
            if let Some(v) = self.value(true)? {
                args.push(v);
            } else if !self.consume(|t| matches!(t, Tk::Ws | Tk::Op(','))) {
                return Err(());
            }
        }
        Ok(Some(args))
    }
}

fn parse(abbr: &str, value_mode: bool) -> Result<Vec<Prop>, ()> {
    let mut p = Parser { toks: tokenize(abbr, value_mode)?, pos: 0 };
    let mut out = Vec::new();
    while p.peek().is_some() {
        if let Some(prop) = p.property(value_mode)? {
            out.push(prop);
        } else if !p.consume(|t| matches!(t, Tk::Op('+'))) {
            return Err(());
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ snippets

enum Snip {
    Raw { key: String, value: String },
    Prop { key: String, property: String, value: Vec<Vec<Value>>, keywords: Vec<(String, T)>, deps: Vec<usize> },
}

impl Snip {
    fn key(&self) -> &str {
        match self {
            Snip::Raw { key, .. } | Snip::Prop { key, .. } => key,
        }
    }
}

fn create_snippet(key: String, value: &str) -> Snip {
    // `^([a-z-]+)(?:\s*:\s*([^\n\r;]+?);*)?$`
    let prop_len = value.find(|c: char| !(c.is_ascii_lowercase() || c == '-')).unwrap_or(value.len());
    let rest = value[prop_len..].trim_start();
    let values = if rest.is_empty() {
        Some(None)
    } else if let Some(v) = rest.strip_prefix(':') {
        let v = v.trim_start().trim_end_matches(';');
        (!v.is_empty() && !v.contains(['\n', '\r', ';'])).then_some(Some(v))
    } else {
        None
    };
    let (Some(values), true) = (values, prop_len > 0) else {
        return Snip::Raw { key, value: value.to_string() };
    };
    let parsed: Vec<Vec<Value>> = values
        .map(|v| v.split('|').map(|alt| parse(alt.trim(), true).ok().and_then(|p| p.into_iter().next()).map(|p| p.value).unwrap_or_default()).collect())
        .unwrap_or_default();
    let mut keywords: Vec<(String, T)> = Vec::new();
    let mut add = |k: String, tok: T| match keywords.iter_mut().find(|(n, _)| *n == k) {
        Some(e) => e.1 = tok,
        None => keywords.push((k, tok)),
    };
    for alt in &parsed {
        for value in alt {
            for tok in value {
                match &tok.v {
                    V::Lit(l) => add(l.clone(), tok.clone()),
                    V::Func { name, .. } => add(name.clone(), tok.clone()),
                    V::Field { name, .. } if !name.trim().is_empty() => add(name.trim().to_string(), t(V::Lit(name.trim().to_string()))),
                    _ => {}
                }
            }
        }
    }
    Snip::Prop { key, property: value[..prop_len].to_string(), value: parsed, keywords, deps: Vec::new() }
}

fn snippets() -> &'static Vec<Snip> {
    static S: OnceLock<Vec<Snip>> = OnceLock::new();
    S.get_or_init(|| {
        let map = super::load_snippets(include_str!("data/css.json"));
        let mut out: Vec<Snip> = map.into_iter().map(|(k, v)| create_snippet(k, &v)).collect();
        out.sort_by(|a, b| a.key().cmp(b.key()));
        // A property's dependencies are the longer properties it starts (`border` → `border-top`).
        let mut stack: Vec<usize> = Vec::new();
        for i in 0..out.len() {
            let Snip::Prop { property: cur, .. } = &out[i] else { continue };
            let cur = cur.clone();
            while let Some(&top) = stack.last() {
                let Snip::Prop { property: prev, .. } = &out[top] else { unreachable!() };
                if cur.starts_with(prev.as_str()) && cur[prev.len()..].starts_with('-') {
                    if let Snip::Prop { deps, .. } = &mut out[top] {
                        deps.push(i);
                    }
                    stack.push(i);
                    break;
                }
                stack.pop();
            }
            if stack.is_empty() {
                stack.push(i);
            }
        }
        out
    })
}

/// Emmet's fuzzy score of `abbr` against `s`: 1 for equal, 0 for no match.
fn score(abbr: &str, s: &str, partial: bool) -> f64 {
    let a: Vec<char> = abbr.to_lowercase().chars().collect();
    let b: Vec<char> = s.to_lowercase().chars().collect();
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() || a[0] != b[0] {
        return 0.0;
    }
    if !partial && a.len() > b.len() {
        return 0.0;
    }
    let min = a.len().min(b.len()) as f64;
    let max = a.len().max(b.len()) as f64;
    let (mut i, mut j) = (1, 1);
    let mut total = max;
    while i < a.len() {
        let mut found = false;
        let mut acronym = false;
        while j < b.len() {
            if a[i] == b[j] {
                found = true;
                total += max - if acronym { i } else { j } as f64;
                break;
            }
            acronym = b[j] == '-';
            j += 1;
        }
        if !found {
            if !partial {
                return 0.0;
            }
            break;
        }
        i += 1;
    }
    let sum = |n: f64| n * (n + 1.0) / 2.0;
    let ratio = i as f64 / max;
    total * ratio / (sum(max) - sum(max - min))
}

fn best_match<'a, I: Clone>(abbr: &str, items: impl Iterator<Item = (&'a str, I)>, partial: bool) -> Option<I> {
    let mut best = None;
    let mut max = 0.0;
    for (key, item) in items {
        let s = score(abbr, key, partial);
        if s == 1.0 {
            return Some(item);
        }
        if s > 0.0 && s >= max {
            max = s;
            best = Some(item);
        }
    }
    best
}

const KEYWORDS: &[&str] = &["auto", "inherit", "unset", "none"];
const UNITLESS: &[&str] = &["z-index", "line-height", "opacity", "font-weight", "zoom", "flex", "flex-grow", "flex-shrink"];

fn resolve_keyword(kw: &str, snippet: Option<usize>) -> Option<T> {
    let all = snippets();
    if let Some(Snip::Prop { keywords, deps, .. }) = snippet.map(|i| &all[i]) {
        if let Some(t) = best_match(kw, keywords.iter().map(|(k, t)| (k.as_str(), t)), false) {
            return Some(t.clone());
        }
        for &d in deps {
            if let Snip::Prop { keywords, .. } = &all[d] {
                if let Some(t) = best_match(kw, keywords.iter().map(|(k, t)| (k.as_str(), t)), false) {
                    return Some(t.clone());
                }
            }
        }
    }
    best_match(kw, KEYWORDS.iter().map(|k| (*k, *k)), false).map(|k| t(V::Lit(k.into())))
}

fn resolve_value_keywords(prop: &mut Prop, snippet: Option<usize>) {
    for value in &mut prop.value {
        for tok in value.iter_mut() {
            match &tok.v {
                V::Lit(l) => {
                    if let Some(k) = resolve_keyword(l, snippet) {
                        *tok = k;
                    }
                }
                V::Func { name, args } => {
                    if let Some(T { v: V::Func { name: mname, args: margs }, .. }) = resolve_keyword(name, snippet) {
                        let mut all = args.clone();
                        all.extend(margs.iter().skip(args.len()).cloned());
                        *tok = t(V::Func { name: mname, args: all });
                    }
                }
                _ => {}
            }
        }
    }
}

fn has_field(value: &Value) -> bool {
    value.iter().any(|t| match &t.v {
        V::Field { .. } => true,
        V::Func { args, .. } => args.iter().any(has_field),
        _ => false,
    })
}

fn wrap_with_field(value: &Value, index: &mut u32) -> Value {
    let mut out = Vec::new();
    let field = |name: String, index: &mut u32| {
        *index += 1;
        t(V::Field { index: *index - 1, name })
    };
    for tok in value {
        match &tok.v {
            V::Color { .. } => out.push(field(color(&tok.v), index)),
            V::Lit(l) => out.push(field(l.clone(), index)),
            V::Num { value, unit, .. } => out.push(field(format!("{value}{unit}"), index)),
            V::Str { value, double } => {
                let q = if *double { '"' } else { '\'' };
                out.push(field(format!("{q}{value}{q}"), index));
            }
            V::Func { name, args } => {
                out.push(field(name.clone(), index));
                out.push(t(V::Lit("(".into())));
                for (i, a) in args.iter().enumerate() {
                    out.extend(wrap_with_field(a, index));
                    if i + 1 != args.len() {
                        out.push(t(V::Lit(", ".into())));
                    }
                }
                out.push(t(V::Lit(")".into())));
            }
            _ => out.push(tok.clone()),
        }
    }
    out
}

/// (unmatched rest of `abbr` after matching its chars in order in `key`)
fn unmatched_part<'a>(abbr: &'a str, key: &str) -> &'a str {
    let key: Vec<char> = key.chars().collect();
    let mut last = 0;
    for (bi, c) in abbr.char_indices() {
        match key[last.min(key.len())..].iter().position(|k| *k == c) {
            Some(p) => last += p + 1,
            None => return &abbr[bi..],
        }
    }
    ""
}

fn resolve_as_property(mut prop: Prop, idx: usize) -> Option<Prop> {
    let Snip::Prop { key, property, value, .. } = &snippets()[idx] else { return None };
    let abbr = prop.name.clone()?;
    let inline = unmatched_part(&abbr, key);
    if !inline.is_empty() {
        if !prop.value.is_empty() {
            return None;
        }
        prop.value.push(vec![resolve_keyword(inline, Some(idx))?]);
    }
    prop.name = Some(property.clone());
    if !prop.value.is_empty() {
        resolve_value_keywords(&mut prop, Some(idx));
    } else if let Some(default) = value.first() {
        prop.value = if value.len() == 1 || default.iter().any(has_field) {
            default.clone()
        } else {
            let mut index = 1;
            default.iter().map(|v| wrap_with_field(v, &mut index)).collect()
        };
    }
    Some(prop)
}

fn resolve_as_snippet(mut prop: Prop, raw: &str) -> Prop {
    let mut input: std::collections::VecDeque<T> = prop.value.first().cloned().unwrap_or_default().into();
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        let close = after.find('}');
        let (Some(close), true) = (close, digits > 0) else {
            out.push(t(V::Lit(rest[..start + 2].to_string())));
            rest = &rest[start + 2..];
            continue;
        };
        let spec = &after[..close];
        if !(spec.len() == digits || spec[digits..].starts_with(':') && spec.len() > digits + 1) {
            out.push(t(V::Lit(rest[..start + 2].to_string())));
            rest = &rest[start + 2..];
            continue;
        }
        if start > 0 {
            out.push(t(V::Lit(rest[..start].to_string())));
        }
        match input.pop_front() {
            Some(tok) => out.push(tok),
            None => out.push(t(V::Field { index: spec[..digits].parse().unwrap_or(0), name: spec.get(digits + 1..).unwrap_or("").to_string() })),
        }
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        out.push(t(V::Lit(rest.to_string())));
    }
    prop.name = None;
    prop.value = vec![out];
    prop
}

/// Resolves a property against the snippets. `None` when nothing matched (those
/// aren't suggested: `abc` would become `abc: ;`).
fn resolve(mut prop: Prop) -> Option<Prop> {
    // `lg(...)`: a linear gradient.
    let lg = match prop.value.as_slice() {
        [v] => match v.as_slice() {
            [T { v: V::Func { name, args }, .. }] if name == "lg" => Some(args.clone()),
            _ => None,
        },
        _ => None,
    };
    if lg.is_some() || prop.name.as_deref() == Some("lg") {
        let args = lg.unwrap_or_else(|| vec![vec![t(V::Field { index: 0, name: String::new() })]]);
        prop.name = Some("background-image".into());
        prop.value = vec![vec![t(V::Func { name: "linear-gradient".into(), args })]];
        return Some(prop);
    }
    let name = prop.name.clone()?;
    let all = snippets();
    let idx = best_match(&name, all.iter().enumerate().map(|(i, s)| (s.key(), i)), true)?;
    let mut prop = match &all[idx] {
        Snip::Prop { .. } => resolve_as_property(prop, idx)?,
        Snip::Raw { value, .. } => resolve_as_snippet(prop, value),
    };
    if let Some(name) = &prop.name {
        for v in &mut prop.value {
            for tok in v.iter_mut() {
                if let V::Num { value, raw, unit } = &mut tok.v {
                    if !unit.is_empty() {
                        *unit = match unit.as_str() {
                            "e" => "em",
                            "p" => "%",
                            "x" => "ex",
                            "r" => "rem",
                            u => u,
                        }
                        .to_string();
                    } else if *value != 0.0 && !UNITLESS.contains(&name.as_str()) {
                        *unit = if raw.contains('.') { "em" } else { "px" }.into();
                    }
                }
            }
        }
    }
    Some(prop)
}

// ------------------------------------------------------------------ output

fn frac(n: f64, digits: usize) -> String {
    let s = format!("{n:.digits$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn color(v: &V) -> String {
    let V::Color { r, g, b, a } = *v else { return String::new() };
    if r == 0 && g == 0 && b == 0 && a == 0.0 {
        return "transparent".into();
    }
    if a == 1.0 {
        if [r, g, b].iter().all(|c| c % 17 == 0) {
            return format!("#{:x}{:x}{:x}", r >> 4, g >> 4, b >> 4);
        }
        return format!("#{r:02x}{g:02x}{b:02x}");
    }
    format!("rgba({r}, {g}, {b}, {})", frac(a, 8))
}

fn output_token(tok: &T, out: &mut Out) {
    match &tok.v {
        V::Color { .. } => out.text(&color(&tok.v)),
        V::Lit(s) | V::Custom(s) => out.text(s),
        V::Num { value, unit, .. } => out.text(&format!("{}{unit}", frac(*value, 4))),
        V::Str { value, double } => {
            let q = if *double { '"' } else { '\'' };
            out.text(&format!("{q}{value}{q}"));
        }
        V::Field { index, name } => out.field(*index, name),
        V::Func { name, args } => {
            out.text(&format!("{name}("));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.text(", ");
                }
                output_value(a, out);
            }
            out.text(")");
        }
    }
}

fn output_value(value: &Value, out: &mut Out) {
    let mut prev_end = None;
    for (i, tok) in value.iter().enumerate() {
        let adjacent = matches!(tok.v, V::Field { .. }) && tok.span.map(|s| s.0) == prev_end;
        if i > 0 && !adjacent {
            out.text(" ");
        }
        output_token(tok, out);
        prev_end = tok.span.map(|s| s.1);
    }
}

/// Expands a stylesheet abbreviation to snippet text; None if it doesn't parse or match.
pub(super) fn expand(abbr: &str) -> Option<String> {
    let props = parse(abbr, false).ok()?;
    if props.is_empty() {
        return None;
    }
    let props: Vec<Prop> = props.into_iter().map(resolve).collect::<Option<_>>()?;
    let mut out = Out::default();
    for (i, p) in props.iter().enumerate() {
        if i > 0 {
            out.newline(0);
        }
        match &p.name {
            Some(name) => {
                out.text(&format!("{name}: "));
                if p.value.is_empty() {
                    out.field(0, "");
                }
                for (i, v) in p.value.iter().enumerate() {
                    if i > 0 {
                        out.text(", ");
                    }
                    output_value(v, &mut out);
                }
                if p.important {
                    out.text(" !important");
                }
                out.text(";");
            }
            None => {
                for tok in p.value.iter().flatten() {
                    output_token(tok, &mut out);
                }
                if p.important {
                    out.text(if p.value.is_empty() { "!important" } else { " !important" });
                }
            }
        }
    }
    Some(out.text_value())
}
