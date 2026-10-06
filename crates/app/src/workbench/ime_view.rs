//! Input methods on the workbench's side: the text being composed, drawn at the focused caret,
//! and committed text typed into whatever has the keyboard (see `crate::ime`).

use render::{Canvas, Rect};

use super::Workbench;
use crate::ime::{self, Preedit};
use crate::input::{Key, KeyInput};

impl Workbench {
    /// The input method's marked text changed (empty: composing ended or was cancelled).
    /// `selected` is the part it's working on, in bytes.
    pub fn ime_preedit(&mut self, text: String, selected: Option<(usize, usize)>) {
        self.preedit = Preedit { text, selected };
    }

    /// The input method finished composing `text`: it's typed like keys would be, so each
    /// target treats it as typing (one undo step in the editor, filtering in the palette...).
    pub fn ime_commit(&mut self, text: String) {
        self.preedit = Preedit::default();
        if text.is_empty() {
            return;
        }
        let key = KeyInput { key: Key::Char(text.clone()), text: Some(text), cmd: false, shift: false, alt: false, ctrl: false };
        self.key(key);
    }

    /// Where the window's input method should put its candidate list: the focused caret (and
    /// the text being composed). None when nothing takes text, which turns input methods off.
    pub fn ime_area(&self) -> Option<Rect> {
        self.ime_area
    }

    /// Draws the composed text at the caret recorded this frame: over what follows the caret,
    /// underlined, with the part being converted underlined heavier and the input method's caret.
    pub(super) fn draw_preedit(&mut self, c: &mut Canvas) {
        let Some(caret) = ime::current() else {
            self.ime_area = None;
            return;
        };
        let p = &self.preedit;
        if p.text.is_empty() {
            self.ime_area = Some(caret.rect);
            return;
        }
        c.push_layer();
        c.push_clip(caret.clip);
        let st = caret.style;
        let w = c.measure(&p.text, &st).ceil();
        let r = Rect::new(caret.rect.x, caret.rect.y, w + 2.0, caret.rect.h);
        let bg = if caret.background.a > 0.0 { caret.background } else { self.color("input.background") };
        // The rest of the line moves over to make room.
        let covered = if caret.after.is_empty() { r.w } else { (caret.clip.right() - r.x).max(r.w) };
        c.fill(Rect::new(r.x, r.y, covered, r.h), bg);
        c.text(r.x, r.y, &p.text, &st);
        if !caret.after.is_empty() {
            c.text(r.x + w, r.y, &caret.after, &st);
        }
        let base = r.bottom() - 2.0;
        c.fill(Rect::new(r.x, base, w, 1.0), st.color);
        let x_at = |c: &mut Canvas, i: usize| p.text.get(..i).map_or(w, |s| c.measure(s, &st));
        match p.selected {
            Some((a, z)) if a < z => {
                let (xa, xz) = (x_at(c, a), x_at(c, z));
                c.fill(Rect::new(r.x + xa, base - 1.0, xz - xa, 2.0), st.color);
            }
            Some((a, _)) => {
                let x = x_at(c, a);
                c.fill(Rect::new((r.x + x).round(), r.y + 2.0, 1.0, r.h - 4.0), self.color("editorCursor.foreground"));
            }
            None => {}
        }
        c.pop_clip();
        self.ime_area = Some(r);
    }
}
