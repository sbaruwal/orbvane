//! Platform-neutral keyboard input, translated from winit events in `main.rs`.

use crate::commands::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable key, lowercase and without modifiers applied (used for shortcuts).
    Char(String),
    Space,
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// Function key F1–F20.
    F(u8),
    Other,
}

#[derive(Clone, Debug)]
pub struct KeyInput {
    pub key: Key,
    /// Text the key produces with modifiers applied (e.g. "P" for shift+p), if any.
    pub text: Option<String>,
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl KeyInput {
    /// The shortcut key name ("p", "space", "f12"), if this key can be part of a shortcut.
    /// Plain keys type text; only modified keys and function keys can be shortcuts. (A ⌥ key
    /// that matches no command still types its character.)
    fn shortcut_key(&self) -> Option<String> {
        let modified = self.cmd || self.ctrl;
        Some(match &self.key {
            Key::Char(c) if modified || self.alt => c.clone(),
            Key::Space if modified => "space".to_string(),
            Key::F(n) => format!("f{n}"),
            Key::Enter => "enter".into(),
            Key::Escape if modified || self.alt => "escape".into(),
            Key::Tab if modified || self.alt => "tab".into(),
            Key::Backspace if modified || self.alt => "backspace".into(),
            Key::Delete if modified || self.alt => "delete".into(),
            Key::Home if modified || self.alt => "home".into(),
            Key::End if modified || self.alt => "end".into(),
            Key::PageUp if modified || self.alt => "pageup".into(),
            Key::PageDown if modified || self.alt => "pagedown".into(),
            Key::Up if modified || self.alt => "up".into(),
            Key::Down if modified || self.alt => "down".into(),
            Key::Left if modified || self.alt => "left".into(),
            Key::Right if modified || self.alt => "right".into(),
            _ => return None,
        })
    }

    pub(crate) fn is(&self, s: &crate::commands::Shortcut) -> bool {
        self.shortcut_key().is_some_and(|k| s.matches(&k, self.cmd, self.shift, self.alt, self.ctrl))
    }

    /// The command bound to this key combination, if any.
    pub fn command(&self) -> Option<Command> {
        crate::keymap::command_for(self)
    }

    /// Whether this key starts a chord (like ⌘K in ⌘K ⌘T).
    pub fn starts_chord(&self) -> bool {
        crate::keymap::starts_chord(self)
    }

    /// The command for `first` followed by this key, if any.
    pub fn chord_command(&self, first: &KeyInput) -> Option<Command> {
        crate::keymap::chord_command(first, self)
    }

    /// This key as a stroke to bind (Keyboard Shortcuts' recorder): any key, with modifiers.
    pub fn stroke(&self) -> Option<crate::commands::Shortcut> {
        let name = match &self.key {
            Key::Char(c) => c.clone(),
            Key::Space => "space".into(),
            Key::Enter => "enter".into(),
            Key::Tab => "tab".into(),
            Key::Backspace => "backspace".into(),
            Key::Delete => "delete".into(),
            Key::Escape => "escape".into(),
            Key::Left => "left".into(),
            Key::Right => "right".into(),
            Key::Up => "up".into(),
            Key::Down => "down".into(),
            Key::Home => "home".into(),
            Key::End => "end".into(),
            Key::PageUp => "pageup".into(),
            Key::PageDown => "pagedown".into(),
            Key::F(n) => format!("f{n}"),
            Key::Other => return None,
        };
        Some(crate::commands::Shortcut { cmd: self.cmd, shift: self.shift, alt: self.alt, ctrl: self.ctrl, key: crate::keymap::intern(&name) })
    }

    /// Keycap label of this key, for the "waiting for second key" status message.
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (on, sym) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.cmd, "⌘")] {
            if on {
                s.push_str(sym);
            }
        }
        match &self.key {
            Key::Char(c) => s.push_str(&c.to_uppercase()),
            Key::F(n) => s.push_str(&format!("F{n}")),
            Key::Up => s.push('↑'),
            Key::Down => s.push('↓'),
            _ => {}
        }
        s
    }
}
