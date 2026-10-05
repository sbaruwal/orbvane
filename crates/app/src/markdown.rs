//! A Markdown parser for the preview (`workbench/markdown_view.rs`): CommonMark's common
//! blocks (headings, paragraphs, lists with tasks, fenced and indented code, quotes, tables,
//! rules) and inlines (emphasis, strong, strikethrough, code, links, images, autolinks).

/// A run of text with one style.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Inline {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// The link's target (for images, the image's).
    pub link: Option<String>,
    pub image: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// A list: ordered (with its first number) or not, and its items.
    List { start: Option<u64>, items: Vec<ListItem> },
    Code { lang: String, lines: Vec<String> },
    Quote(Vec<Block>),
    /// Header cells, then rows; each cell's inlines. Alignments aren't kept.
    Table(Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>),
    Rule,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    /// A task list item: checked or not.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Parses a Markdown document.
pub fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    parse_lines(&lines)
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && indent_of(line) < 4 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let t = line.trim_start();
    if indent_of(line) >= 4 {
        return None;
    }
    let level = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((level as u8, rest.trim().trim_end_matches('#').trim_end()))
}

/// A fence line: its marker (``` or ~~~, possibly longer) and info string.
fn fence(line: &str) -> Option<(String, &str)> {
    let t = line.trim_start();
    for ch in ['`', '~'] {
        let n = t.chars().take_while(|c| *c == ch).count();
        if n >= 3 {
            return Some((ch.to_string().repeat(n), t[n..].trim()));
        }
    }
    None
}

/// A list marker: (is ordered, its number, the content's column, the rest of the line).
fn list_marker(line: &str) -> Option<(bool, u64, usize, &str)> {
    let ind = indent_of(line);
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        if rest.starts_with(' ') || rest.is_empty() {
            let content = rest.trim_start();
            return Some((false, 0, ind + 1 + (rest.len() - content.len()).max(1), content));
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let rest = &t[digits..];
        if let Some(after) = rest.strip_prefix(['.', ')']) {
            if after.starts_with(' ') || after.is_empty() {
                let content = after.trim_start();
                let n = t[..digits].parse().unwrap_or(1);
                return Some((true, n, ind + digits + 1 + (after.len() - content.len()).max(1), content));
            }
        }
    }
    None
}

fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| "|-: ".contains(c)) && t.contains('|')
}

fn table_cells(line: &str) -> Vec<Vec<Inline>> {
    let t = line.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| inlines(c.trim())).collect()
}

fn parse_lines(lines: &[&str]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
            continue;
        }
        if let Some((marker, info)) = fence(line) {
            let lang = info.split_whitespace().next().unwrap_or_default().to_string();
            let base = indent_of(line);
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(&marker) {
                // Content loses the fence's indentation.
                let l = lines[i];
                let strip = indent_of(l).min(base);
                code.push(l.chars().skip(strip).collect());
                i += 1;
            }
            i += 1; // the closing fence
            out.push(Block::Code { lang, lines: code });
            continue;
        }
        if indent_of(line) >= 4 {
            let mut code = Vec::new();
            while i < lines.len() && (indent_of(lines[i]) >= 4 || is_blank(lines[i])) {
                code.push(lines[i].chars().skip(4).collect::<String>());
                i += 1;
            }
            while code.last().is_some_and(|l: &String| l.trim().is_empty()) {
                code.pop();
            }
            out.push(Block::Code { lang: String::new(), lines: code });
            continue;
        }
        if let Some((level, text)) = heading(line) {
            out.push(Block::Heading(level, inlines(text)));
            i += 1;
            continue;
        }
        if is_rule(line) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if line.trim_start().starts_with('>') {
            let mut inner = Vec::new();
            while i < lines.len() && !is_blank(lines[i]) {
                let t = lines[i].trim_start();
                inner.push(t.strip_prefix('>').map(|r| r.strip_prefix(' ').unwrap_or(r)).unwrap_or(t));
                i += 1;
            }
            out.push(Block::Quote(parse_lines(&inner)));
            continue;
        }
        if let Some((ordered, n, _, _)) = list_marker(line) {
            let base = indent_of(line);
            let mut items = Vec::new();
            while i < lines.len() {
                let Some((o, _, content_col, first)) = list_marker(lines[i]).filter(|_| indent_of(lines[i]) == base) else { break };
                if o != ordered {
                    break;
                }
                let mut body: Vec<String> = vec![first.to_string()];
                i += 1;
                // The item's other lines: indented under its content, or lazy continuations.
                while i < lines.len() {
                    let l = lines[i];
                    if is_blank(l) {
                        if i + 1 < lines.len() && indent_of(lines[i + 1]) >= content_col {
                            body.push(String::new());
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    if indent_of(l) >= content_col {
                        body.push(l.chars().skip(content_col).collect());
                    } else if list_marker(l).is_none() && heading(l).is_none() && fence(l).is_none() && !is_rule(l) && indent_of(l) > base {
                        body.push(l.trim_start().to_string());
                    } else {
                        break;
                    }
                    i += 1;
                }
                let mut task = None;
                if let Some(first) = body.first_mut() {
                    for (mark, checked) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                        if let Some(rest) = first.strip_prefix(mark) {
                            task = Some(checked);
                            *first = rest.to_string();
                        }
                    }
                }
                let refs: Vec<&str> = body.iter().map(String::as_str).collect();
                items.push(ListItem { task, blocks: parse_lines(&refs) });
                // A blank line between items doesn't end the list.
                while i < lines.len() && is_blank(lines[i]) && i + 1 < lines.len() && list_marker(lines[i + 1]).is_some_and(|m| m.0 == ordered) && indent_of(lines[i + 1]) == base {
                    i += 1;
                }
            }
            out.push(Block::List { start: ordered.then_some(n), items });
            continue;
        }
        if line.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let header = table_cells(line);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].contains('|') && !is_blank(lines[i]) {
                rows.push(table_cells(lines[i]));
                i += 1;
            }
            out.push(Block::Table(header, rows));
            continue;
        }
        // A paragraph: up to a blank line or another kind of block. A line of "===" or
        // "---" under it makes it a heading.
        let mut text = vec![line.trim()];
        i += 1;
        let mut setext = None;
        while i < lines.len() && !is_blank(lines[i]) {
            let l = lines[i];
            let t = l.trim();
            if !t.is_empty() && t.chars().all(|c| c == '=') {
                setext = Some(1);
                i += 1;
                break;
            }
            if !t.is_empty() && t.chars().all(|c| c == '-') && text.len() == 1 {
                setext = Some(2);
                i += 1;
                break;
            }
            if heading(l).is_some() || fence(l).is_some() || is_rule(l) || t.starts_with('>') || list_marker(l).is_some() {
                break;
            }
            text.push(t);
            i += 1;
        }
        // A line ending in two spaces or a backslash is a hard break; others join with a space.
        let joined = text.join(" ");
        let body = inlines(&joined);
        out.push(match setext {
            Some(level) => Block::Heading(level, body),
            None => Block::Paragraph(body),
        });
    }
    out
}

/// Parses inline Markdown: `code`, **strong**, *emphasis*, ~~strikethrough~~, [links](url),
/// ![images](src), <autolinks> and bare URLs.
pub fn inlines(text: &str) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    parse_inlines(text, &Inline::default(), &mut out);
    // Merge neighbors with the same style.
    let mut merged: Vec<Inline> = Vec::new();
    for run in out.into_iter().filter(|r| !r.text.is_empty()) {
        match merged.last_mut() {
            Some(last) if Inline { text: String::new(), ..last.clone() } == Inline { text: String::new(), ..run.clone() } => last.text.push_str(&run.text),
            _ => merged.push(run),
        }
    }
    merged
}

fn parse_inlines(text: &str, style: &Inline, out: &mut Vec<Inline>) {
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<Inline>| {
        if !plain.is_empty() {
            out.push(Inline { text: std::mem::take(plain), ..style.clone() });
        }
    };
    let rest = |i: usize| -> String { chars[i..].iter().collect() };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Backslash escapes.
        if c == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() {
            plain.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '`' {
            let ticks = chars[i..].iter().take_while(|&&c| c == '`').count();
            let marker: String = "`".repeat(ticks);
            let after = rest(i + ticks);
            if let Some(end) = after.find(&marker) {
                flush(&mut plain, out);
                let code = after[..end].trim().to_string();
                out.push(Inline { text: code, code: true, ..style.clone() });
                i += ticks + after[..end].chars().count() + ticks;
                continue;
            }
        }
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((label, url, len)) = link_at(&chars, i + 1) {
                flush(&mut plain, out);
                out.push(Inline { text: label, image: true, link: Some(url), ..style.clone() });
                i += 1 + len;
                continue;
            }
        }
        if c == '[' {
            if let Some((label, url, len)) = link_at(&chars, i) {
                flush(&mut plain, out);
                parse_inlines(&label, &Inline { link: Some(url), ..style.clone() }, out);
                i += len;
                continue;
            }
        }
        if c == '<' {
            let after = rest(i + 1);
            if let Some(end) = after.find('>') {
                let inner = &after[..end];
                if inner.starts_with("http://") || inner.starts_with("https://") || inner.starts_with("mailto:") {
                    flush(&mut plain, out);
                    out.push(Inline { text: inner.to_string(), link: Some(inner.to_string()), ..style.clone() });
                    i += 2 + inner.chars().count();
                    continue;
                }
            }
        }
        if (c == 'h') && style.link.is_none() && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let after = rest(i);
            if after.starts_with("http://") || after.starts_with("https://") {
                let url: String = after.chars().take_while(|c| !c.is_whitespace() && *c != '<' && *c != '>').collect();
                let url = url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']).to_string();
                flush(&mut plain, out);
                i += url.chars().count();
                out.push(Inline { text: url.clone(), link: Some(url), ..style.clone() });
                continue;
            }
        }
        // Emphasis: **strong**, __strong__, *em*, _em_, ~~strike~~.
        if c == '*' || c == '_' || c == '~' {
            let run = chars[i..].iter().take_while(|&&x| x == c).count();
            let n = if c == '~' { if run >= 2 { 2 } else { 0 } } else { run.min(3) };
            // `_` only counts at a word boundary (snake_case stays as written).
            let boundary = c != '_' || i == 0 || !chars[i - 1].is_alphanumeric();
            if n > 0 && boundary && chars.get(i + n).is_some_and(|x| !x.is_whitespace()) {
                let marker: String = std::iter::repeat_n(c, n).collect();
                let after = rest(i + n);
                // The closing marker, not right after whitespace.
                let close = after.match_indices(&marker).map(|(b, _)| b).find(|&b| b > 0 && !after[..b].ends_with(' ') && (c != '_' || !after[b + marker.len()..].starts_with(|x: char| x.is_alphanumeric())));
                if let Some(end) = close {
                    flush(&mut plain, out);
                    let inner = &after[..end];
                    let mut s = style.clone();
                    match (c, n) {
                        ('~', _) => s.strike = true,
                        (_, 1) => s.italic = true,
                        (_, 2) => s.bold = true,
                        _ => {
                            s.bold = true;
                            s.italic = true;
                        }
                    }
                    parse_inlines(inner, &s, out);
                    i += n + inner.chars().count() + n;
                    continue;
                }
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, out);
}

/// `[label](url)` starting at `chars[i]` (a '['): (label, url, length in chars).
fn link_at(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let mut depth = 0;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    if j >= chars.len() || chars.get(j + 1) != Some(&'(') {
        return None;
    }
    let close = (j + 2..chars.len()).find(|&k| chars[k] == ')')?;
    let label: String = chars[i + 1..j].iter().collect();
    let target: String = chars[j + 2..close].iter().collect();
    // A title after the URL ("url \"title\"") isn't part of it.
    let url = target.split_whitespace().next().unwrap_or_default().trim_matches(['<', '>']).to_string();
    Some((label, url, close + 1 - i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(runs: &[Inline]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn blocks() {
        let md = "# Title\n\nSome *text*\nmore.\n\n- one\n- [x] two\n  1. nested\n\n```rust\nfn main() {}\n```\n\n> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\nSub\n===\n";
        let b = parse(md);
        assert!(matches!(&b[0], Block::Heading(1, t) if text(t) == "Title"));
        assert!(matches!(&b[1], Block::Paragraph(t) if text(t) == "Some text more."));
        let Block::List { start: None, items } = &b[2] else { panic!("{:?}", b[2]) };
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].task, Some(true));
        assert!(matches!(&items[1].blocks[1], Block::List { start: Some(1), .. }));
        assert!(matches!(&b[3], Block::Code { lang, lines } if lang == "rust" && lines == &["fn main() {}"]));
        assert!(matches!(&b[4], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(t) if text(t) == "quoted")));
        assert!(matches!(&b[5], Block::Table(h, rows) if h.len() == 2 && rows.len() == 1));
        assert_eq!(b[6], Block::Rule);
        assert!(matches!(&b[7], Block::Heading(1, t) if text(t) == "Sub"));
    }

    #[test]
    fn inline_styles() {
        let r = inlines("a **bold** _em_ `code` ~~gone~~ [link](https://x.io \"t\") snake_case_name ![img](p.png) https://y.io.");
        let find = |t: &str| r.iter().find(|x| x.text == t).unwrap_or_else(|| panic!("{t} in {r:?}")).clone();
        assert!(find("bold").bold);
        assert!(find("em").italic);
        assert!(find("code").code);
        assert!(find("gone").strike);
        assert_eq!(find("link").link.as_deref(), Some("https://x.io"));
        assert!(find("img").image);
        assert_eq!(find("https://y.io").link.as_deref(), Some("https://y.io"));
        assert!(text(&r).contains("snake_case_name"));
    }
}
