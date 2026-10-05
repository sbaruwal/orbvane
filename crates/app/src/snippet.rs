//! The snippet syntax: `$1`, `${1:placeholder}` (nested),
//! `${1|one,two|}` choices, `$0` for the final cursor, variables like `$TM_FILENAME`, and `\`
//! escapes. Parsing gives the text to insert and where each tab stop is in it. Transforms
//! (`${1/re/fmt/}`) are read and ignored.

/// A parsed snippet: the text, and each tab stop's ranges (byte offsets into `text`), ordered
/// as Tab visits them (1, 2, ..., then 0, the final cursor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub stops: Vec<Vec<std::ops::Range<usize>>>,
}

/// Values for variables (`$TM_FILENAME`...). Unknown ones insert their name as a
/// placeholder.
pub trait Variables {
    fn get(&self, name: &str) -> Option<String>;
}

impl Variables for () {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

struct Parser<'a, V: Variables> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    out: String,
    /// (index, range) for each stop occurrence.
    stops: Vec<(u32, std::ops::Range<usize>)>,
    vars: &'a V,
}

impl<V: Variables> Parser<'_, V> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.src[start..self.i].parse().ok()
    }

    fn name(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
            self.i += 1;
        }
        self.src[start..self.i].to_string()
    }

    /// Text until `}` (not consumed) or the end, handling nested snippet syntax. `in_choice`:
    /// also stop at `,` and `|`.
    fn body(&mut self, until_brace: bool) {
        while let Some(c) = self.peek() {
            match c {
                b'}' if until_brace => return,
                b'\\' => {
                    let next = self.s.get(self.i + 1).copied();
                    match next {
                        Some(b'$' | b'}' | b'\\') => {
                            self.out.push(next.unwrap() as char);
                            self.i += 2;
                        }
                        _ => {
                            self.out.push('\\');
                            self.i += 1;
                        }
                    }
                }
                b'$' => self.dollar(),
                _ => {
                    // Copy one UTF-8 char.
                    let len = self.src[self.i..].chars().next().map_or(1, char::len_utf8);
                    self.out.push_str(&self.src[self.i..self.i + len]);
                    self.i += len;
                }
            }
        }
    }

    fn dollar(&mut self) {
        self.i += 1; // $
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let n = self.number().unwrap_or(0);
                let at = self.out.len();
                self.stops.push((n, at..at));
            }
            Some(b'{') => {
                self.i += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    let n = self.number().unwrap_or(0);
                    let start = self.out.len();
                    match self.peek() {
                        Some(b':') => {
                            self.i += 1;
                            self.body(true);
                        }
                        Some(b'|') => {
                            // A choice: insert the first option.
                            self.i += 1;
                            let rest = &self.src[self.i..];
                            let end = rest.find("|}").unwrap_or(rest.len());
                            let first = rest[..end].split(',').next().unwrap_or("");
                            self.out.push_str(first);
                            self.i += end + 1;
                        }
                        Some(b'/') => self.skip_transform(),
                        _ => {}
                    }
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                    }
                    self.stops.push((n, start..self.out.len()));
                } else {
                    let name = self.name();
                    let start = self.out.len();
                    let value = self.vars.get(&name);
                    match self.peek() {
                        Some(b':') => {
                            self.i += 1;
                            let before = self.out.len();
                            self.body(true);
                            if let Some(v) = &value {
                                // The variable has a value: it replaces the default.
                                self.out.truncate(before);
                                self.out.push_str(v);
                            }
                        }
                        Some(b'/') => {
                            self.skip_transform();
                            self.out.push_str(value.as_deref().unwrap_or(""));
                        }
                        _ => self.out.push_str(value.as_deref().unwrap_or(&name)),
                    }
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                    }
                    if value.is_none() {
                        // Unknown variable: a placeholder with its name (after numbered stops).
                        self.stops.push((u32::MAX - 1, start..self.out.len()));
                    }
                }
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let name = self.name();
                let start = self.out.len();
                match self.vars.get(&name) {
                    Some(v) => self.out.push_str(&v),
                    None => {
                        self.out.push_str(&name);
                        self.stops.push((u32::MAX - 1, start..self.out.len()));
                    }
                }
            }
            _ => self.out.push('$'),
        }
    }

    /// Skips `/regex/format/options` up to the closing `}`.
    fn skip_transform(&mut self) {
        let mut slashes = 0;
        while let Some(c) = self.peek() {
            match c {
                b'\\' => self.i += 2,
                b'/' => {
                    slashes += 1;
                    self.i += 1;
                }
                b'}' if slashes >= 3 => return,
                _ => self.i += 1,
            }
        }
    }
}

pub fn parse(src: &str, vars: &impl Variables) -> Snippet {
    let mut p = Parser { s: src.as_bytes(), src, i: 0, out: String::new(), stops: Vec::new(), vars };
    p.body(false);
    let (out, mut stops) = (p.out, p.stops);
    // Tab order: 1, 2, ... (unknown variables after them), then 0 (or the end) last.
    let final_stop = stops.iter().filter(|(n, _)| *n == 0).map(|(_, r)| r.clone()).collect::<Vec<_>>();
    stops.retain(|(n, _)| *n != 0);
    stops.sort_by_key(|(n, r)| (*n, r.start));
    let mut ordered: Vec<Vec<std::ops::Range<usize>>> = Vec::new();
    let mut last: Option<u32> = None;
    for (n, r) in stops {
        // Unknown variables are separate stops even when they share the marker number.
        if last == Some(n) && n != u32::MAX - 1 {
            ordered.last_mut().unwrap().push(r);
        } else {
            ordered.push(vec![r]);
        }
        last = Some(n);
    }
    ordered.push(if final_stop.is_empty() { vec![out.len()..out.len()] } else { final_stop });
    Snippet { text: out, stops: ordered }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_and_placeholders() {
        let s = parse("fn ${1:name}(${2:args}) {\n\t$0\n}", &());
        assert_eq!(s.text, "fn name(args) {\n\t\n}");
        assert_eq!(s.stops[0], vec![3..7]);
        assert_eq!(s.stops[1], vec![8..12]);
        assert_eq!(s.stops[2], vec![17..17]);
    }

    #[test]
    fn mirrors_choices_escapes_and_final_default() {
        let s = parse("${1:x} = $1 + ${2|a,b|} \\$5", &());
        assert_eq!(s.text, "x =  + a $5");
        assert_eq!(s.stops[0], vec![0..1, 4..4]);
        assert_eq!(s.stops[1], vec![7..8]);
        // No $0: the final stop is at the end.
        assert_eq!(s.stops[2], vec![s.text.len()..s.text.len()]);
    }

    #[test]
    fn nested_and_variables() {
        struct V;
        impl Variables for V {
            fn get(&self, name: &str) -> Option<String> {
                (name == "TM_FILENAME").then(|| "main.rs".into())
            }
        }
        let s = parse("${1:outer ${2:inner}} $TM_FILENAME ${UNKNOWN}", &V);
        assert_eq!(s.text, "outer inner main.rs UNKNOWN");
        assert_eq!(s.stops[0], vec![0..11]);
        assert_eq!(s.stops[1], vec![6..11]);
        assert_eq!(s.stops[2], vec![20..27]);
    }
}
