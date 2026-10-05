//! Reusable controls: a single-line text field, and text wrapping.

use render::{Canvas, Color, Rect, TextStyle};

use crate::input::{Key, KeyInput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldEvent {
    /// The key wasn't used by the field.
    Ignored,
    /// The caret or selection moved.
    Moved,
    /// The text changed.
    Changed,
}

/// A single-line text input with caret, selection and horizontal scrolling.
#[derive(Default)]
pub struct TextField {
    pub text: String,
    /// Caret and selection anchor as char indices.
    caret: usize,
    anchor: usize,
    scroll: f32,
    /// x offsets of each char boundary from the last draw, for mouse hit-testing.
    offsets: Vec<f32>,
    origin_x: f32,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl TextField {
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map_or(self.text.len(), |(b, _)| b)
    }

    /// Replaces the text, placing the caret at the end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.caret = self.len();
        self.anchor = self.caret;
    }

    /// Selects chars `anchor..caret` (the caret ends at `caret`).
    pub fn select_range(&mut self, anchor: usize, caret: usize) {
        self.anchor = anchor.min(self.len());
        self.caret = caret.min(self.len());
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.len();
    }

    fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn selected_text(&self) -> String {
        let (a, b) = self.selection();
        self.text[self.byte(a)..self.byte(b)].to_string()
    }

    /// Replaces the selection with `s`.
    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        let (a, b) = self.selection();
        let (ba, bb) = (self.byte(a), self.byte(b));
        self.text.replace_range(ba..bb, &s);
        self.caret = a + s.chars().count();
        self.anchor = self.caret;
    }

    fn word_left(&self, from: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = from;
        while i > 0 && !is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = from;
        while i < chars.len() && !is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        i
    }

    fn move_to(&mut self, pos: usize, extend: bool) {
        self.caret = pos.min(self.len());
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Handles editing and caret keys. Clipboard shortcuts arrive as commands; see
    /// `copy`, `cut` and `paste`.
    pub fn key(&mut self, k: &KeyInput) -> FieldEvent {
        let (a, b) = self.selection();
        let has_sel = a != b;
        match &k.key {
            Key::Left if !k.shift && has_sel && !k.cmd && !k.alt => self.move_to(a, false),
            Key::Right if !k.shift && has_sel && !k.cmd && !k.alt => self.move_to(b, false),
            Key::Left => {
                let to = if k.cmd { 0 } else if k.alt { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
                self.move_to(to, k.shift);
            }
            Key::Right => {
                let to = if k.cmd { self.len() } else if k.alt { self.word_right(self.caret) } else { self.caret + 1 };
                self.move_to(to, k.shift);
            }
            Key::Home => self.move_to(0, k.shift),
            Key::End => self.move_to(self.len(), k.shift),
            Key::Backspace => {
                if !has_sel {
                    self.anchor = if k.cmd { 0 } else if k.alt { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
                }
                if self.selection().0 == self.selection().1 {
                    return FieldEvent::Moved;
                }
                self.insert("");
                return FieldEvent::Changed;
            }
            Key::Delete => {
                if !has_sel {
                    self.anchor = if k.alt { self.word_right(self.caret) } else { (self.caret + 1).min(self.len()) };
                }
                if self.selection().0 == self.selection().1 {
                    return FieldEvent::Moved;
                }
                self.insert("");
                return FieldEvent::Changed;
            }
            Key::Char(_) | Key::Space if !k.cmd && !k.ctrl => {
                let Some(t) = &k.text else { return FieldEvent::Ignored };
                self.insert(t);
                return FieldEvent::Changed;
            }
            _ => return FieldEvent::Ignored,
        }
        FieldEvent::Moved
    }

    pub fn copy(&self) -> Option<String> {
        let s = self.selected_text();
        (!s.is_empty()).then_some(s)
    }

    pub fn cut(&mut self) -> Option<String> {
        let s = self.copy()?;
        self.insert("");
        Some(s)
    }

    /// Places the caret at window x (from the last draw).
    pub fn click(&mut self, x: f32, extend: bool) {
        let rel = x - self.origin_x;
        let pos = self
            .offsets
            .windows(2)
            .position(|w| rel < (w[0] + w[1]) / 2.0)
            .unwrap_or(self.offsets.len().saturating_sub(1));
        self.move_to(pos, extend);
    }

    /// Draws the text inside `r` (which the caller has already styled as a box).
    pub fn draw(&mut self, c: &mut Canvas, r: Rect, style: &TextStyle, placeholder: &str, placeholder_color: Color, focused: bool, caret_on: bool, selection_color: Color) {
        // Measure each char boundary for hit-testing and caret placement.
        self.offsets.clear();
        self.offsets.push(0.0);
        let mut prefix = String::new();
        for ch in self.text.chars() {
            prefix.push(ch);
            self.offsets.push(c.measure(&prefix, style));
        }
        let caret_x = self.offsets[self.caret.min(self.offsets.len() - 1)];
        // Keep the caret in view.
        if caret_x - self.scroll > r.w - 4.0 {
            self.scroll = caret_x - r.w + 4.0;
        } else if caret_x < self.scroll {
            self.scroll = caret_x;
        }
        let total = *self.offsets.last().unwrap();
        self.scroll = self.scroll.min((total - r.w + 4.0).max(0.0)).max(0.0);
        self.origin_x = r.x - self.scroll;

        c.push_clip(r);
        let y = r.y + ((r.h - style.line_height) / 2.0).round();
        if self.text.is_empty() {
            c.text(r.x, y, placeholder, &style.color(placeholder_color));
        } else {
            let (a, b) = self.selection();
            if a != b && focused {
                let (xa, xb) = (self.offsets[a], self.offsets[b]);
                c.fill(Rect::new(self.origin_x + xa, r.y + 3.0, xb - xa, r.h - 6.0), selection_color);
            }
            c.text(self.origin_x, y, &self.text, style);
        }
        if focused && caret_on {
            c.fill(Rect::new((self.origin_x + caret_x).round(), r.y + 4.0, 1.0, r.h - 8.0), style.color);
        }
        c.pop_clip();
    }
}

/// Breaks `text` into lines no wider than `width` at spaces (long words overflow).
pub fn wrap(c: &mut Canvas, text: &str, style: &TextStyle, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if !line.is_empty() && c.measure(&candidate, style) > width {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key) -> KeyInput {
        KeyInput { key, text: None, cmd: false, shift: false, alt: false, ctrl: false }
    }

    fn typed(s: &str) -> KeyInput {
        KeyInput { text: Some(s.into()), ..key(Key::Char(s.into())) }
    }

    #[test]
    fn edits() {
        let mut f = TextField::default();
        for ch in ["f", "o", "o", " ", "b", "a", "r"] {
            assert_eq!(f.key(&typed(ch)), FieldEvent::Changed);
        }
        assert_eq!(f.text, "foo bar");
        f.key(&KeyInput { alt: true, ..key(Key::Backspace) });
        assert_eq!(f.text, "foo ");
        f.key(&KeyInput { shift: true, ..key(Key::Home) });
        assert_eq!(f.selected_text(), "foo ");
        f.key(&typed("x"));
        assert_eq!(f.text, "x");
        f.set_text("héllo");
        f.key(&key(Key::Left));
        f.key(&key(Key::Backspace));
        assert_eq!(f.text, "hélo");
    }
}
