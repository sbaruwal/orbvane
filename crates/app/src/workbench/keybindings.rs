//! Keyboard Shortcuts (⌘K ⌘S): every command with its keybinding in the quick input; picking
//! one records a new key ("Press desired key combination and then press Enter"),
//! which is added to `keybindings.json`. The file itself opens with "Open Keyboard Shortcuts
//! (JSON)"; saving it (or changing it outside the editor) applies it.

use super::{Effect, Focus, GitInput, Workbench};
use crate::commands::Command;
use crate::input::{Key, KeyInput};
use crate::keymap;
use crate::palette::{Action, InputBox, Item, Palette, Picker};

const PROMPT: &str = "Press desired key combination and then press Enter.";

impl Workbench {
    pub(super) fn open_keyboard_shortcuts(&mut self) {
        let choices = Command::all()
            .into_iter()
            .map(|cmd| Item {
                label: cmd.title().to_string(),
                detail: cmd.id().to_string(),
                matches: Vec::new(),
                shortcut: keymap::keycaps(cmd),
                action: Action::DefineKeybinding(cmd),
                group: None,
                kind: None,
            })
            .collect();
        self.palette = Some(Palette::with_picker(Picker { placeholder: "Type to search in keybindings".into(), choices }));
    }

    pub(super) fn open_keybindings_json(&mut self) {
        let path = keymap::path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, keymap::TEMPLATE);
        }
        self.open_file(&path);
        self.focus = Focus::Editor;
    }

    /// Asks for the key to bind to `cmd`.
    pub(super) fn define_keybinding(&mut self, cmd: Command) {
        let b = InputBox {
            prompt: format!("{PROMPT}\n{}", cmd.title()),
            placeholder: String::new(),
            purpose: GitInput::Keybinding(cmd),
            error: None,
            password: false,
        };
        self.palette = Some(Palette::with_input(b, ""));
    }

    /// While recording: each key press (with its modifiers) becomes the binding; a second
    /// one makes a chord. Enter saves, Escape cancels. Returns true if the palette is recording.
    pub(super) fn record_key(&mut self, k: &KeyInput) -> bool {
        let Some(p) = &mut self.palette else { return false };
        let Some(GitInput::Keybinding(cmd)) = p.input_box.as_ref().map(|b| b.purpose.clone()) else { return false };
        let plain = !(k.cmd || k.ctrl || k.alt || k.shift);
        match k.key {
            Key::Escape if plain => {
                self.cancel_palette();
                return true;
            }
            Key::Enter if plain && !p.input.is_empty() => {
                let keys = p.input.clone();
                match keymap::add_user_binding(&keys, cmd) {
                    Ok(()) => {
                        self.palette = None;
                        self.reload_keymap(true);
                    }
                    Err(e) => {
                        if let Some(b) = &mut p.input_box {
                            b.error = Some(format!("Couldn't save keybindings.json: {e}"));
                        }
                    }
                }
                return true;
            }
            _ => {}
        }
        let Some(stroke) = k.stroke() else { return true };
        let stroke = keymap::format_stroke(&stroke);
        // A second stroke makes a chord; a third starts over.
        p.input = if p.input.is_empty() || p.input.contains(' ') { stroke } else { format!("{} {stroke}", p.input) };
        let taken: Vec<&str> = keymap::parse_keys(&p.input)
            .map(|(f, s)| keymap::all().into_iter().filter(|b| b.first == f && b.second == s && b.command != cmd).map(|b| b.command.title()).collect())
            .unwrap_or_default();
        if let Some(b) = &mut p.input_box {
            b.prompt = match taken.len() {
                0 => format!("{PROMPT}\n{}", cmd.title()),
                1 => format!("{PROMPT}\n1 existing command has this keybinding: {}", taken[0]),
                n => format!("{PROMPT}\n{n} existing commands have this keybinding"),
            };
        }
        true
    }

    /// Re-reads `keybindings.json` (if it changed, or always with `force`) and updates the
    /// menus' shortcuts.
    pub(super) fn reload_keymap(&mut self, force: bool) {
        let result = if force { Some(keymap::load()) } else { keymap::reload_if_changed() };
        match result {
            None => return,
            Some(Err(e)) => self.set_status_message(&e),
            Some(Ok(())) => {}
        }
        self.effects.push(Effect::KeymapChanged);
    }
}
