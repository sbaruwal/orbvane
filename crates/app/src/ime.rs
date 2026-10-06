//! Input methods (composing Chinese, Japanese or Korean text, accented letters...). Whatever has
//! the keyboard records its caret while it's drawn; after the frame the workbench draws the text
//! being composed there, and the window learns where to put the input method's candidate list.
//! No caret recorded = nothing to type into, so the input method is switched off.

use std::cell::RefCell;

use render::{Color, Rect, TextStyle};

/// The caret of the focused text target, as drawn this frame.
#[derive(Clone)]
pub struct Caret {
    /// The caret's line (its height is the text's).
    pub rect: Rect,
    /// The text's style, so composed text looks like what it will become.
    pub style: TextStyle,
    /// What's behind the text.
    pub background: Color,
    /// The rest of the caret's line, drawn again after the composed text (which pushes it right).
    pub after: String,
    /// Where the text target's text may be drawn.
    pub clip: Rect,
}

thread_local! {
    static CARET: RefCell<Option<Caret>> = const { RefCell::new(None) };
}

/// Forgets the last frame's caret; called as a frame starts.
pub fn begin_frame() {
    CARET.with(|c| *c.borrow_mut() = None);
}

/// Records the focused caret. Called whether or not the caret is blinked on; the last call in a
/// frame wins (overlays draw after what they cover).
pub fn caret(rect: Rect, style: &TextStyle, background: Color, after: &str, clip: Rect) {
    CARET.with(|c| *c.borrow_mut() = Some(Caret { rect, style: *style, background, after: after.to_string(), clip }));
}

/// The caret recorded this frame.
pub fn current() -> Option<Caret> {
    CARET.with(|c| c.borrow().clone())
}

/// Text being composed: the input method's marked text and the part it has selected
/// (byte offsets into the text).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preedit {
    pub text: String,
    pub selected: Option<(usize, usize)>,
}
