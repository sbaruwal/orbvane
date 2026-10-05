//! Editing a JSONC settings file in place: set or remove one top-level property while keeping
//! the user's comments, ordering and formatting, like `jsonc-parser` edits.

/// A top-level property of the settings object, as byte ranges into the text.
#[derive(Debug)]
struct Member {
    key: String,
    /// Start of the key's opening quote.
    start: usize,
    value_start: usize,
    value_end: usize,
    /// The comma after the value, if any.
    comma: Option<usize>,
}

/// The top-level object: its braces and properties. None if the text isn't an object.
struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

/// Skips whitespace and comments from `i`.
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i = match find(b, i + 2, b"*/") {
                Some(e) => e + 2,
                None => b.len(),
            };
        } else {
            return i;
        }
    }
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    (from..b.len().saturating_sub(pat.len() - 1)).find(|&i| b[i..].starts_with(pat))
}

/// End of the string starting at the quote `i` (index after the closing quote).
fn string_end(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// End of the JSON value starting at `i`.
fn value_end(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(b'"') => string_end(b, i),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j);
                        continue;
                    }
                    b'/' if b.get(j + 1) == Some(&b'/') || b.get(j + 1) == Some(&b'*') => {
                        j = skip_trivia(b, j);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            b.len()
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b'/') && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            j
        }
    }
}

fn parse_object(text: &str) -> Option<Object> {
    let b = text.as_bytes();
    let open = skip_trivia(b, 0);
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut members = Vec::new();
    let mut i = open + 1;
    loop {
        i = skip_trivia(b, i);
        match b.get(i)? {
            b'}' => return Some(Object { open, close: i, members }),
            b'"' => {
                let start = i;
                let key_end = string_end(b, i);
                let key: String = serde_json::from_str(&text[start..key_end]).ok()?;
                i = skip_trivia(b, key_end);
                if b.get(i) != Some(&b':') {
                    return None;
                }
                let value_start = skip_trivia(b, i + 1);
                let value_end = value_end(b, value_start);
                i = skip_trivia(b, value_end);
                let comma = (b.get(i) == Some(&b',')).then_some(i);
                if comma.is_some() {
                    i += 1;
                }
                members.push(Member { key, start, value_start, value_end, comma });
            }
            b',' => i += 1, // stray comma
            _ => return None,
        }
    }
}

/// The indentation of the first property, or four spaces.
fn indent_unit(text: &str, obj: &Object) -> String {
    obj.members
        .first()
        .and_then(|m| {
            let line_start = text[..m.start].rfind('\n').map_or(0, |i| i + 1);
            let ws = &text[line_start..m.start];
            (!ws.is_empty() && ws.chars().all(|c| c == ' ' || c == '\t')).then(|| ws.to_string())
        })
        .unwrap_or_else(|| "    ".into())
}

/// Indents every line after the first of a pretty-printed value by `indent`.
fn indent_value(value: &str, indent: &str) -> String {
    value.lines().enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { format!("{indent}{l}") }).collect::<Vec<_>>().join("\n")
}

/// Returns `text` with top-level property `key` set to `value` (serialized JSON), or removed
/// when `value` is None. Text that isn't an object is replaced by a new object.
pub fn set_property(text: &str, key: &str, value: Option<&serde_json::Value>) -> String {
    let Some(obj) = parse_object(text) else {
        return match value {
            Some(v) => format!("{{\n    {}: {}\n}}\n", serde_json::to_string(key).unwrap(), indent_value(&pretty(v, "    "), "    ")),
            None => text.to_string(),
        };
    };
    let indent = indent_unit(text, &obj);
    let key_json = serde_json::to_string(key).unwrap();
    let existing = obj.members.iter().rposition(|m| m.key == key);
    match (existing, value) {
        (Some(i), Some(v)) => {
            let m = &obj.members[i];
            format!("{}{}{}", &text[..m.value_start], indent_value(&pretty(v, &indent), &indent), &text[m.value_end..])
        }
        (Some(i), None) => {
            // Remove the whole line(s) of the property, and fix up the comma.
            let m = &obj.members[i];
            let line_start = text[..m.start].rfind('\n').map_or(m.start, |i| i + 1);
            let line_start = if text[line_start..m.start].trim().is_empty() { line_start } else { m.start };
            let mut end = m.comma.map_or(m.value_end, |c| c + 1);
            // Take a trailing line comment and the newline with the property.
            let rest = &text[end..];
            let line_end = rest.find('\n').map_or(rest.len(), |n| n + 1);
            if rest[..line_end].trim().is_empty() || rest[..line_end].trim_start().starts_with("//") {
                end += line_end;
            }
            let mut out = format!("{}{}", &text[..line_start], &text[end..]);
            // If it was the last property, the previous one must lose its comma.
            if m.comma.is_none() && i > 0 {
                if let Some(c) = obj.members[i - 1].comma {
                    out.replace_range(c..c + 1, "");
                }
            }
            out
        }
        (None, Some(v)) => {
            let entry = format!("{indent}{key_json}: {}", indent_value(&pretty(v, &indent), &indent));
            match obj.members.last() {
                Some(last) => {
                    // Insert after the last property (and its comma, adding one if needed).
                    let (at, comma) = match last.comma {
                        Some(c) => (c + 1, ""),
                        None => (last.value_end, ","),
                    };
                    format!("{}{comma}\n{entry}{}", &text[..at], &text[at..])
                }
                None => {
                    let inner = &text[obj.open + 1..obj.close];
                    let trimmed = if inner.trim().is_empty() { "" } else { inner.trim_end() };
                    format!("{}{}\n{entry}\n{}", &text[..obj.open + 1], trimmed, &text[obj.close..])
                }
            }
        }
        (None, None) => text.to_string(),
    }
}

/// Returns `text` with property `key` of the object in top-level property `section` set (or
/// removed), like `set_property` one level down: a `.code-workspace` file's `settings`.
pub fn set_nested(text: &str, section: &str, key: &str, value: Option<&serde_json::Value>) -> String {
    let inner = parse_object(text).and_then(|obj| {
        let m = obj.members.iter().rev().find(|m| m.key == section)?;
        (text.as_bytes().get(m.value_start) == Some(&b'{')).then_some((m.value_start, m.value_end))
    });
    match inner {
        Some((start, end)) => {
            // Edit the nested object's text; lines added inside it get the parent's indentation.
            let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
            let outer: String = text[line_start..start].chars().take_while(|c| c.is_whitespace()).collect();
            let edited = set_property(&text[start..end], key, value);
            let edited = if edited.contains("\n") && !text[start..end].contains('\n') {
                edited.lines().enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { format!("{outer}{l}") }).collect::<Vec<_>>().join("\n")
            } else {
                edited
            };
            format!("{}{}{}", &text[..start], edited.trim_end_matches('\n'), &text[end..])
        }
        None => match value {
            Some(v) => set_property(text, section, Some(&serde_json::json!({ key: v }))),
            None => text.to_string(),
        },
    }
}

/// Pretty JSON with four-space indentation (serde_json's default is two).
/// `v` pretty-printed, indenting nested levels by `indent` (the file's own unit).
fn pretty(v: &serde_json::Value, indent: &str) -> String {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(v, &mut ser).unwrap();
    String::from_utf8(buf).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(&theme::strip_jsonc(s)).unwrap()
    }

    #[test]
    fn replaces_value_keeping_comments() {
        let src = "{\n    // Font\n    \"editor.fontSize\": 12, // small\n    \"a\": true\n}\n";
        let out = set_property(src, "editor.fontSize", Some(&json!(14)));
        assert_eq!(out, "{\n    // Font\n    \"editor.fontSize\": 14, // small\n    \"a\": true\n}\n");
    }

    #[test]
    fn inserts_with_detected_indent() {
        let src = "{\n\t\"a\": true\n}";
        let out = set_property(src, "b", Some(&json!("x")));
        assert_eq!(out, "{\n\t\"a\": true,\n\t\"b\": \"x\"\n}");
        let out = set_property("{}", "b", Some(&json!(1)));
        assert_eq!(out, "{\n    \"b\": 1\n}");
        let out = set_property("", "b", Some(&json!(1)));
        assert_eq!(parse(&out), json!({"b": 1}));
        // Trailing comma after the last property is kept valid.
        let out = set_property("{\n    \"a\": 1,\n}", "b", Some(&json!(2)));
        assert_eq!(parse(&out), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn edits_nested_settings() {
        let src = "{\n\t\"folders\": [],\n\t\"settings\": {\n\t\t\"a\": 1\n\t}\n}";
        let out = set_nested(src, "settings", "b", Some(&json!(2)));
        assert_eq!(parse(&out), json!({"folders": [], "settings": {"a": 1, "b": 2}}));
        assert!(out.contains("\t\t\"b\": 2"), "{out}");
        let out = set_nested(&out, "settings", "a", None);
        assert_eq!(parse(&out), json!({"folders": [], "settings": {"b": 2}}));
        let out = set_nested("{\n\t\"folders\": []\n}", "settings", "c", Some(&json!(true)));
        assert_eq!(parse(&out), json!({"folders": [], "settings": {"c": true}}));
        let out = set_nested("{\"settings\": {}}", "settings", "d", Some(&json!(1)));
        assert_eq!(parse(&out), json!({"settings": {"d": 1}}));
    }

    #[test]
    fn removes_property_and_fixes_commas() {
        let src = "{\n    \"a\": 1,\n    \"b\": 2,\n    \"c\": 3\n}";
        assert_eq!(set_property(src, "b", None), "{\n    \"a\": 1,\n    \"c\": 3\n}");
        assert_eq!(set_property(src, "c", None), "{\n    \"a\": 1,\n    \"b\": 2\n}");
        assert_eq!(set_property(src, "a", None), "{\n    \"b\": 2,\n    \"c\": 3\n}");
        assert_eq!(parse(&set_property("{\"a\": 1}", "a", None)), json!({}));
        assert_eq!(set_property(src, "zzz", None), src);
    }

    #[test]
    fn nested_values() {
        let src = "{\n    \"files.exclude\": {\n        \"**/.git\": true // hide\n    },\n    \"x\": [1, {\"y\": \"}\"}]\n}";
        let out = set_property(src, "x", Some(&json!(false)));
        assert_eq!(parse(&out), json!({"files.exclude": {"**/.git": true}, "x": false}));
        let out = set_property(src, "files.exclude", Some(&json!({"a": true})));
        assert_eq!(parse(&out), json!({"files.exclude": {"a": true}, "x": [1, {"y": "}"}]}));
        assert!(out.contains("\"files.exclude\": {\n        \"a\": true\n    },"));
    }

    #[test]
    fn nested_values_use_the_files_indent() {
        let out = set_property("{\n\t\"a\": 1\n}", "b", Some(&json!([{"p": 1}])));
        assert!(out.contains("\t\"b\": [\n\t\t{\n\t\t\t\"p\": 1\n\t\t}\n\t]"), "{out:?}");
    }
}
