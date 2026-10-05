//! Emmet abbreviations: `ul>li.item$*3` in HTML/XML/JSX and `m10`,
//! `db` in stylesheets expand to snippet text. A port of Emmet (emmetio/emmet, MIT, with its
//! snippet and lorem data in `data/`) plus the rules for where an abbreviation is found
//! and which ones are worth suggesting.

mod css;
mod lorem;
mod markup;

use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Html,
    Xml,
    Jsx,
    Css,
}

impl Syntax {
    /// The Emmet syntax of a file and its language id (for `emmet.excludeLanguages`).
    pub fn of(path: &Path) -> Option<(Syntax, &'static str)> {
        Some(match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "html" | "htm" | "xhtml" => (Syntax::Html, "html"),
            "vue" => (Syntax::Html, "vue"),
            "svelte" => (Syntax::Html, "svelte"),
            "xml" | "xsl" | "xsd" | "svg" | "plist" => (Syntax::Xml, "xml"),
            "jsx" => (Syntax::Jsx, "javascriptreact"),
            "tsx" => (Syntax::Jsx, "typescriptreact"),
            "css" => (Syntax::Css, "css"),
            "scss" => (Syntax::Css, "scss"),
            "less" => (Syntax::Css, "less"),
            _ => return None,
        })
    }
}

/// An abbreviation before the caret and its expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct Expansion {
    /// Char column where the abbreviation starts on the caret's line.
    pub start: usize,
    pub abbr: String,
    /// snippet text, indented with tabs.
    pub snippet: String,
}

/// The abbreviation before the caret, if the caret is somewhere Emmet works (not inside a tag
/// or a `<script>`, inside a CSS rule). `before` is the document's text before the caret (the
/// tail is enough), `line` the caret's line up to the caret. `suggest` applies the noise
/// filter for completions (plain words that aren't tags, unresolved CSS properties).
pub fn at_caret(syntax: Syntax, before: &str, line: &str, suggest: bool) -> Option<Expansion> {
    let syntax = context(syntax, before)?;
    let chars: Vec<char> = line.chars().collect();
    let start = extract(&chars, syntax)?;
    let abbr: String = chars[start..].iter().collect();
    let prev = start.checked_sub(1).map(|i| chars[i]);
    match syntax {
        Syntax::Css => {
            // A property's place: nothing but whitespace since the last `{`, `;` or `}`.
            let head: String = chars[..start].iter().collect();
            let stmt = before[..before.len() - line.len()].to_string() + &head;
            let from = stmt.rfind(['{', ';', '}']).map_or(0, |i| i + 1);
            if !stmt[from..].trim().is_empty() || !valid_css(&abbr) {
                return None;
            }
        }
        _ => {
            if prev == Some('<') || !valid_markup(&abbr, syntax) {
                return None;
            }
            if syntax == Syntax::Jsx {
                if prev.is_some_and(|c| !(c.is_whitespace() || c == '(' || c == '>' || c == '{')) || in_js_string(&chars[..start]) {
                    return None;
                }
                let name: String = abbr.chars().take_while(|c| !".#[{>+*^(".contains(*c)).collect();
                if name.starts_with(|c: char| c.is_ascii_lowercase()) && !markup::is_known_tag(&name) {
                    return None;
                }
            }
        }
    }
    let snippet = expand(&abbr, syntax)?;
    if suggest && syntax != Syntax::Css && is_noise(&abbr, &snippet, syntax) {
        return None;
    }
    Some(Expansion { start, abbr, snippet })
}

/// Expands `abbr` for `syntax`.
pub fn expand(abbr: &str, syntax: Syntax) -> Option<String> {
    match syntax {
        Syntax::Css => css::expand(abbr),
        _ => markup::expand(abbr, syntax),
    }
}

/// Wraps `text` with a markup abbreviation (Emmet: Wrap with Abbreviation).
pub fn wrap(abbr: &str, text: &str, syntax: Syntax) -> Option<String> {
    if syntax == Syntax::Css || abbr.trim().is_empty() {
        return None;
    }
    markup::wrap(abbr.trim(), text, syntax)
}

/// The syntax at the caret: CSS inside `<style>`, none inside a tag or `<script>` or outside
/// a CSS rule.
fn context(syntax: Syntax, before: &str) -> Option<Syntax> {
    match syntax {
        Syntax::Html | Syntax::Xml => {
            let lower = before.to_ascii_lowercase();
            let opened = |open: &str, close: &str| lower.rfind(open).is_some_and(|o| lower.rfind(close).is_none_or(|c| c < o));
            if syntax == Syntax::Html && opened("<script", "</script") {
                return None;
            }
            if syntax == Syntax::Html && opened("<style", "</style") {
                let from = lower.rfind("<style").unwrap();
                let from = before[from..].find('>').map(|i| from + i + 1)?;
                return context(Syntax::Css, &before[from..]);
            }
            // Inside a tag (or a comment): the last `<` comes after the last `>`.
            if before.rfind('<').is_some_and(|lt| before.rfind('>').is_none_or(|gt| gt < lt)) {
                return None;
            }
            Some(syntax)
        }
        Syntax::Css => {
            let mut depth = 0i32;
            let mut chars = before.chars().peekable();
            let mut quote = None;
            while let Some(c) = chars.next() {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), '\\') => {
                        chars.next();
                    }
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(c),
                    (None, '/') if chars.peek() == Some(&'*') => {
                        let mut prev = ' ';
                        for c in chars.by_ref() {
                            if prev == '*' && c == '/' {
                                break;
                            }
                            prev = c;
                        }
                    }
                    (None, '{') => depth += 1,
                    (None, '}') => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
            (depth > 0).then_some(Syntax::Css)
        }
        Syntax::Jsx => Some(Syntax::Jsx),
    }
}

/// Whether the caret is inside a JS string or `//` comment on this line.
fn in_js_string(line: &[char]) -> bool {
    let mut quote = None;
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) if c == '\\' => i += 1,
            Some(_) => {}
            None if matches!(c, '"' | '\'' | '`') => quote = Some(c),
            None if c == '/' && line.get(i + 1) == Some(&'/') => return true,
            None => {}
        }
        i += 1;
    }
    quote.is_some()
}

fn is_abbr_char(c: char, syntax: Syntax) -> bool {
    if c.is_alphanumeric() {
        return true;
    }
    match syntax {
        Syntax::Css => "-:!#.%+@$,_".contains(c),
        _ => "_-:!$@#.*>+^/%".contains(c),
    }
}

/// Where the abbreviation ending at the end of `line` starts (Emmet's extract-abbreviation):
/// back over abbreviation characters and balanced `[...]`, `{...}` and `(...)`.
fn extract(line: &[char], syntax: Syntax) -> Option<usize> {
    let mut i = line.len();
    let mut stack: Vec<char> = Vec::new();
    while i > 0 {
        let c = line[i - 1];
        if let Some(&open) = stack.last() {
            if c == open {
                stack.pop();
            } else if matches!((open, c), ('{', '}') | ('[', ']') | ('(', ')' | ']' | '}')) {
                stack.push(match c {
                    '}' => '{',
                    ']' => '[',
                    _ => '(',
                });
            }
            i -= 1;
            continue;
        }
        match c {
            ']' => stack.push('['),
            '}' => stack.push('{'),
            ')' => stack.push('('),
            '>' if ends_with_tag(&line[..i]) => break,
            c if is_abbr_char(c, syntax) => {}
            _ => break,
        }
        i -= 1;
    }
    if !stack.is_empty() {
        return None;
    }
    // Operators can't start an abbreviation.
    while i < line.len() && ">+^*/".contains(line[i]) {
        i += 1;
    }
    (i < line.len()).then_some(i)
}

/// Whether `s` ends with an HTML tag (`<div class="a">`, `</p>`), whose `>` isn't a child operator.
fn ends_with_tag(s: &[char]) -> bool {
    let Some(lt) = s.iter().rposition(|&c| c == '<') else { return false };
    let inner: String = s[lt + 1..s.len() - 1].iter().collect();
    let inner = inner.strip_prefix('/').unwrap_or(&inner);
    let name_end = inner.find(|c: char| !(c.is_alphanumeric() || "-_:.".contains(c))).unwrap_or(inner.len());
    name_end > 0
        && inner.starts_with(|c: char| c.is_ascii_alphabetic())
        && inner[name_end..].chars().next().is_none_or(|c| c.is_whitespace() || c == '/')
}

/// `isAbbreviationValid` for markup.
fn valid_markup(abbr: &str, syntax: Syntax) -> bool {
    if abbr.starts_with('!') {
        return abbr.chars().all(|c| c == '!');
    }
    let first = abbr.chars().next().unwrap_or(' ');
    if !(first.is_ascii_alphabetic() || "!([#.".contains(first) || (first == '{' && syntax != Syntax::Jsx)) {
        return false;
    }
    // Parentheses group only next to an operator (people often type "(text)").
    let bare: String = {
        let mut depth = 0;
        abbr.chars()
            .filter(|&c| {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => return depth == 0,
                }
                false
            })
            .collect()
    };
    if bare.contains(['(', ')']) {
        let ops = |s: &str| [">", "+", "*", "^"].iter().any(|o| bare.contains(&s.replace('_', o)));
        if !ops(")_") && !ops("_(") {
            return false;
        }
    }
    true
}

/// `isAbbreviationValid` for stylesheets.
fn valid_css(abbr: &str) -> bool {
    if let Some(hash) = abbr.find('#') {
        let (head, tail) = (&abbr[..hash], &abbr[hash + 1..]);
        let head_ok = head.is_empty() || head.trim_end_matches(':').chars().all(|c| c.is_ascii_alphabetic()) && !head.trim_end_matches(':').is_empty();
        return head_ok && tail.len() <= 6 && tail.chars().all(|c| c.is_ascii_hexdigit() || (c == '.' && !head.is_empty()));
    }
    let mut cs = abbr.chars();
    let first = match cs.next() {
        Some('-') => cs.next(),
        c => c,
    };
    first.is_some_and(|c| c.is_ascii_alphabetic() || "!@#".contains(c))
}

/// `isExpandedTextNoise` for markup: a plain word that just becomes a tag of that
/// name, unless it's a real tag (or a snippet, a custom element name, a JSX component).
fn is_noise(abbr: &str, snippet: &str, syntax: Syntax) -> bool {
    if markup::is_known_tag(abbr) {
        return false;
    }
    if abbr.contains(['-', ':']) && !abbr.contains("--") && !abbr.contains("::") && !abbr.ends_with(':') {
        return false;
    }
    if abbr == "." {
        return false;
    }
    if let Some(word) = abbr.strip_suffix('.').filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric())) {
        return !(!word.is_empty() && markup::is_known_tag(word));
    }
    if syntax == Syntax::Jsx && abbr.starts_with(|c: char| c.is_ascii_uppercase()) && abbr.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let lower = abbr.to_lowercase();
    snippet.to_lowercase() == format!("<{lower}>${{1}}</{lower}>")
}

// ------------------------------------------------------------------ shared helpers

/// Snippet text being built: text is escaped for snippets, fields are numbered in the
/// order they first appear.
#[derive(Default)]
struct Out {
    buf: String,
    level: isize,
    line: usize,
    fields: Vec<(u32, u32)>,
}

impl Out {
    /// Text; its line breaks are indented to the current level.
    fn text(&mut self, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.newline(self.level);
            }
            for c in part.trim_end_matches('\r').chars() {
                if matches!(c, '$' | '}' | '\\') {
                    self.buf.push('\\');
                }
                self.buf.push(c);
            }
        }
    }

    fn newline(&mut self, indent: isize) {
        self.buf.push('\n');
        self.line += 1;
        for _ in 0..indent.max(0) {
            self.buf.push('\t');
        }
    }

    fn field(&mut self, index: u32, placeholder: &str) {
        let n = match self.fields.iter().find(|f| f.0 == index) {
            Some(f) => f.1,
            None => {
                let n = self.fields.len() as u32 + 1;
                self.fields.push((index, n));
                n
            }
        };
        if placeholder.is_empty() {
            self.buf.push_str(&format!("${{{n}}}"));
        } else {
            self.buf.push_str(&format!("${{{n}:"));
            for c in placeholder.chars() {
                if matches!(c, '$' | '}' | '\\') {
                    self.buf.push('\\');
                }
                self.buf.push(c);
            }
            self.buf.push('}');
        }
    }

    fn text_value(self) -> String {
        self.buf
    }
}

/// Emmet's snippet files: `"a|b": value` defines both names.
fn load_snippets(json: &str) -> HashMap<String, String> {
    let map: HashMap<String, String> = serde_json::from_str(json).unwrap_or_default();
    let mut out = HashMap::new();
    for (keys, value) in map {
        for k in keys.split('|') {
            out.insert(k.to_string(), value.clone());
        }
    }
    out
}

/// `text` without its common indentation and surrounding blank lines.
fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let lines: Vec<&str> = lines.iter().map(|l| if l.len() >= indent { &l[indent..] } else { l.trim_start() }).collect();
    lines.join("\n").trim_matches('\n').trim_end().to_string()
}

/// A small xorshift generator for lorem ipsum.
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Rng(seed | 1)
    }

    /// `floor(random * (to - from) + from)`, like Emmet's `rand`.
    fn range(&mut self, from: usize, to: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        if to <= from {
            return from;
        }
        from + (self.0 % (to - from) as u64) as usize
    }
}

#[cfg(test)]
mod tests;
