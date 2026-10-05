//! Brackets: which `()`, `[]` and `{}` belong together (outside strings and comments), for
//! bracket pair colorization, highlighting the matching bracket, and Go to Bracket.

use language::{highlight_line, Lang, LineState};
use text::{Buffer, Pos};
use theme::Token;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bracket {
    pub pos: Pos,
    pub open: bool,
    /// Nesting depth (0 for outermost); None for a closing bracket with no opener.
    pub depth: Option<usize>,
    /// Index of the matching bracket.
    pub partner: Option<usize>,
}

/// Documents above this many lines don't get bracket colors.
const MAX_LINES: usize = 100_000;

fn pair(c: char) -> Option<(char, bool)> {
    Some(match c {
        '(' => (')', true),
        '[' => (']', true),
        '{' => ('}', true),
        ')' => ('(', false),
        ']' => ('[', false),
        '}' => ('{', false),
        _ => return None,
    })
}

/// All brackets of a document, rebuilt when it changes.
#[derive(Default)]
pub struct BracketIndex {
    version: Option<u64>,
    pub brackets: Vec<Bracket>,
    /// Index of the first bracket on each line (and one past the end).
    line_start: Vec<usize>,
}

impl BracketIndex {
    pub fn update(&mut self, b: &Buffer, lang: Lang) {
        if self.version == Some(b.version()) {
            return;
        }
        self.version = Some(b.version());
        self.brackets.clear();
        self.line_start.clear();
        if b.len_lines() > MAX_LINES || lang == Lang::PlainText || lang == Lang::Markdown {
            self.line_start.resize(b.len_lines() + 1, 0);
            return;
        }
        let mut state = LineState::Normal;
        let mut spans = Vec::new();
        // Open brackets not closed yet: (index, char).
        let mut stack: Vec<(usize, char)> = Vec::new();
        for line in 0..b.len_lines() {
            self.line_start.push(self.brackets.len());
            let text = b.line(line);
            spans.clear();
            state = highlight_line(lang, &text, state, &mut spans);
            let skip = |byte: usize| spans.iter().any(|(a, z, t)| *a <= byte && byte < *z && matches!(t, Token::String | Token::Comment));
            for (col, (byte, c)) in text.char_indices().enumerate() {
                let Some((other, open)) = pair(c) else { continue };
                if skip(byte) {
                    continue;
                }
                let i = self.brackets.len();
                let pos = Pos::new(line, col);
                if open {
                    self.brackets.push(Bracket { pos, open, depth: Some(stack.len()), partner: None });
                    stack.push((i, c));
                } else if stack.last().is_some_and(|(_, o)| *o == other) {
                    let (o, _) = stack.pop().unwrap();
                    self.brackets[o].partner = Some(i);
                    self.brackets.push(Bracket { pos, open, depth: Some(stack.len()), partner: Some(o) });
                } else {
                    self.brackets.push(Bracket { pos, open, depth: None, partner: None });
                }
            }
        }
        self.line_start.push(self.brackets.len());
    }

    /// Brackets on `line`, in order.
    pub fn on_line(&self, line: usize) -> &[Bracket] {
        match (self.line_start.get(line), self.line_start.get(line + 1)) {
            (Some(&a), Some(&z)) => &self.brackets[a..z],
            _ => &[],
        }
    }

    fn index_at(&self, pos: Pos) -> Option<usize> {
        let a = *self.line_start.get(pos.line)?;
        self.on_line(pos.line).iter().position(|b| b.pos == pos).map(|i| a + i)
    }

    /// The bracket pair next to the cursor at `pos` (the one after it first, then before), as
    /// (this bracket, its partner).
    pub fn pair_at(&self, pos: Pos) -> Option<(Pos, Pos)> {
        let before = pos.col.checked_sub(1).map(|c| Pos::new(pos.line, c));
        [Some(pos), before].into_iter().flatten().find_map(|p| {
            let i = self.index_at(p)?;
            let partner = self.brackets[i].partner?;
            Some((p, self.brackets[partner].pos))
        })
    }

    /// The innermost pair enclosing `pos`: (opening bracket, closing bracket).
    pub fn enclosing(&self, pos: Pos) -> Option<(Pos, Pos)> {
        self.brackets
            .iter()
            .filter(|b| b.open && b.pos < pos)
            .filter_map(|b| b.partner.map(|p| (b.pos, self.brackets[p].pos)))
            .filter(|(_, close)| *close >= pos)
            .last()
    }
}

impl crate::editor::EditorState {
    /// Go to Bracket (⇧⌘\): to the partner of the bracket next to the cursor, or else to the
    /// closing bracket of the pair around it.
    pub fn jump_to_bracket(&mut self, doc: &crate::editor::Doc) {
        let idx = &doc.brackets;
        let heads: Vec<text::Selection> = self
            .selections()
            .into_iter()
            .map(|s| {
                let to = match idx.pair_at(s.head) {
                    Some((_, partner)) => partner,
                    None => match idx.enclosing(s.head) {
                        Some((_, close)) => close,
                        None => s.head,
                    },
                };
                text::Selection::caret(to)
            })
            .collect();
        self.set_selections(heads);
        self.reveal = true;
    }

    /// Select to Bracket: selects each pair around the cursors, brackets included.
    pub fn select_to_bracket(&mut self, doc: &crate::editor::Doc) {
        let idx = &doc.brackets;
        let sels: Vec<text::Selection> = self
            .selections()
            .into_iter()
            .map(|s| {
                let pair = idx.pair_at(s.head).map(|(a, b)| if a < b { (a, b) } else { (b, a) }).or_else(|| idx.enclosing(s.head));
                match pair {
                    Some((open, close)) => text::Selection { anchor: open, head: Pos::new(close.line, close.col + 1), goal_col: None },
                    None => s,
                }
            })
            .collect();
        self.set_selections(sels);
        self.reveal = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        let mut b = Buffer::new();
        b.insert(text::Selection::default(), text);
        b
    }

    #[test]
    fn matches_and_depths_ignoring_strings_and_comments() {
        let b = buffer("fn f(a: [u8; 2]) {\n    let s = \"(\"; // )\n    g(a[0]);\n}\n)");
        let mut idx = BracketIndex::default();
        idx.update(&b, Lang::Rust);
        let depths: Vec<(usize, usize, Option<usize>)> = idx.brackets.iter().map(|b| (b.pos.line, b.pos.col, b.depth)).collect();
        assert_eq!(
            depths,
            vec![
                (0, 4, Some(0)),
                (0, 8, Some(1)),
                (0, 14, Some(1)),
                (0, 15, Some(0)),
                (0, 17, Some(0)),
                (2, 5, Some(1)),
                (2, 7, Some(2)),
                (2, 9, Some(2)),
                (2, 10, Some(1)),
                (3, 0, Some(0)),
                (4, 0, None), // unexpected
            ]
        );
        // Cursor after `)` of `f(...)` and before `{`.
        assert_eq!(idx.pair_at(Pos::new(0, 16)), Some((Pos::new(0, 15), Pos::new(0, 4))));
        assert_eq!(idx.pair_at(Pos::new(0, 17)), Some((Pos::new(0, 17), Pos::new(3, 0))));
        assert_eq!(idx.enclosing(Pos::new(2, 8)), Some((Pos::new(2, 7), Pos::new(2, 9))));
        assert_eq!(idx.enclosing(Pos::new(1, 4)), Some((Pos::new(0, 17), Pos::new(3, 0))));
    }
}
