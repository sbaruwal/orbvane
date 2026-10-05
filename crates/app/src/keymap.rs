//! Keyboard shortcuts: the defaults (`Command::shortcut` / `Command::chord`) with the
//! user's `keybindings.json` on top, in the standard format: an array of
//! `{ "key": "cmd+k cmd+s", "command": "workbench.action.openGlobalKeybindings" }`, where a
//! `"-command"` entry removes that command's binding (for `key`, or all of them). Later
//! bindings win, so the user's override the defaults. `when` clauses are ignored.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::commands::{Command, Shortcut};
use crate::input::KeyInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub first: Shortcut,
    /// The second stroke of a chord (⌘K ⌘S).
    pub second: Option<Shortcut>,
    pub command: Command,
}

thread_local! {
    static KEYMAP: RefCell<Vec<Binding>> = RefCell::new(defaults());
}

/// The built-in bindings, then the extensions' (`contributes.keybindings`).
fn defaults() -> Vec<Binding> {
    let mut out = builtin_defaults();
    for entry in crate::contributions::keybindings() {
        apply(&mut out, &entry);
    }
    out
}

fn builtin_defaults() -> Vec<Binding> {
    let mut out = Vec::new();
    for &command in Command::ALL {
        if let Some(first) = command.shortcut() {
            out.push(Binding { first, second: None, command });
        }
        if let Some((first, second)) = command.chord() {
            out.push(Binding { first, second: Some(second), command });
        }
    }
    out
}

fn with<T>(f: impl FnOnce(&[Binding]) -> T) -> T {
    KEYMAP.with(|k| f(&k.borrow()))
}

/// The single-stroke command for this key.
pub fn command_for(k: &KeyInput) -> Option<Command> {
    with(|b| b.iter().rev().find(|b| b.second.is_none() && k.is(&b.first)).map(|b| b.command))
}

/// Whether this key starts a chord.
pub fn starts_chord(k: &KeyInput) -> bool {
    with(|b| b.iter().any(|b| b.second.is_some() && k.is(&b.first)))
}

/// The command for `first` followed by `second`.
pub fn chord_command(first: &KeyInput, second: &KeyInput) -> Option<Command> {
    with(|b| b.iter().rev().find(|b| b.second.is_some_and(|s| first.is(&b.first) && second.is(&s))).map(|b| b.command))
}

/// The binding shown for a command (the user's, if they set one).
pub fn binding_of(command: Command) -> Option<Binding> {
    with(|b| b.iter().rev().find(|b| b.command == command).copied())
}

/// Every binding, in order (defaults first).
pub fn all() -> Vec<Binding> {
    with(<[Binding]>::to_vec)
}

/// Keycaps for the palette: a chord's two strokes separated by an empty cap.
pub fn keycaps(command: Command) -> Option<Vec<String>> {
    let b = binding_of(command)?;
    let mut caps = b.first.keycaps();
    if let Some(s) = b.second {
        caps.push(String::new());
        caps.extend(s.keycaps());
    }
    Some(caps)
}

/// The accelerator for the native menu (single strokes only; menus can't show chords).
pub fn accelerator(command: Command) -> Option<String> {
    with(|b| b.iter().rev().find(|b| b.command == command && b.second.is_none()).map(|b| b.first.accelerator()))
}

pub fn path() -> PathBuf {
    settings::user_data_dir().join("User").join("keybindings.json")
}

/// Key names as `&'static str` (Shortcut's type), leaked once each.
pub(crate) fn intern(s: &str) -> &'static str {
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap();
    let names = names.get_or_insert_with(HashSet::new);
    if let Some(n) = names.get(s) {
        return n;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    names.insert(leaked);
    leaked
}

/// One stroke in the standard syntax ("cmd+shift+p", "ctrl+`", "f12").
fn parse_stroke(s: &str) -> Option<Shortcut> {
    let mut sc = Shortcut { cmd: false, shift: false, alt: false, ctrl: false, key: "" };
    let parts: Vec<&str> = s.split('+').collect();
    // "cmd++" binds the plus key.
    let (mods, key) = if s.ends_with("++") { (&parts[..parts.len() - 2], "+") } else { (&parts[..parts.len() - 1], *parts.last()?) };
    for m in mods {
        match m.trim().to_lowercase().as_str() {
            "cmd" | "meta" | "win" => sc.cmd = true,
            "shift" => sc.shift = true,
            "alt" | "option" | "opt" => sc.alt = true,
            "ctrl" => sc.ctrl = true,
            _ => return None,
        }
    }
    let key = key.trim().to_lowercase();
    let key = match key.as_str() {
        "esc" => "escape".to_string(),
        "return" => "enter".to_string(),
        "del" => "delete".to_string(),
        k if k.is_empty() => return None,
        k => k.to_string(),
    };
    sc.key = intern(&key);
    Some(sc)
}

/// A key sequence: one stroke or a chord of two ("cmd+k cmd+s").
pub fn parse_keys(s: &str) -> Option<(Shortcut, Option<Shortcut>)> {
    let mut strokes = s.split_whitespace();
    let first = parse_stroke(strokes.next()?)?;
    let second = match strokes.next() {
        Some(t) => Some(parse_stroke(t)?),
        None => None,
    };
    strokes.next().is_none().then_some((first, second))
}

/// A stroke in the standard syntax.
pub fn format_stroke(s: &Shortcut) -> String {
    let mut out = String::new();
    for (on, name) in [(s.ctrl, "ctrl+"), (s.shift, "shift+"), (s.alt, "alt+"), (s.cmd, "cmd+")] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(s.key);
    out
}

thread_local! {
    static LOADED_MTIME: RefCell<Option<std::time::SystemTime>> = const { RefCell::new(None) };
}

/// Reloads if `keybindings.json` changed since the last load. None: unchanged.
pub fn reload_if_changed() -> Option<Result<(), String>> {
    let mtime = std::fs::metadata(path()).and_then(|m| m.modified()).ok();
    if LOADED_MTIME.with(|m| *m.borrow() == mtime) {
        return None;
    }
    Some(load())
}

/// Rebuilds the keymap from the defaults and `keybindings.json`. Returns a message for the
/// user when the file can't be read.
pub fn load() -> Result<(), String> {
    let mut map = defaults();
    LOADED_MTIME.with(|m| *m.borrow_mut() = std::fs::metadata(path()).and_then(|m| m.modified()).ok());
    let result = match std::fs::read_to_string(path()) {
        Err(_) => Ok(()), // no file: the defaults
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&theme::strip_jsonc(&text)) {
            Err(e) => Err(format!("keybindings.json: {e}")),
            Ok(serde_json::Value::Array(entries)) => {
                for e in &entries {
                    apply(&mut map, e);
                }
                Ok(())
            }
            Ok(_) => Err("keybindings.json: expected an array of keybindings".into()),
        },
    };
    KEYMAP.with(|k| *k.borrow_mut() = map);
    result
}

fn apply(map: &mut Vec<Binding>, entry: &serde_json::Value) {
    let Some(id) = entry["command"].as_str() else { return };
    let keys = entry["key"].as_str().and_then(parse_keys);
    if let Some(id) = id.strip_prefix('-') {
        let Some(command) = Command::from_id(id) else { return };
        map.retain(|b| b.command != command || keys.is_some_and(|(f, s)| b.first != f || b.second != s));
        return;
    }
    let (Some(command), Some((first, second))) = (Command::from_id(id), keys) else { return };
    map.push(Binding { first, second, command });
}

/// Adds a binding to `keybindings.json` (creating it), keeping the rest of the file as is.
pub fn add_user_binding(keys: &str, command: Command) -> std::io::Result<()> {
    let path = path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| TEMPLATE.to_string());
    let new = with_binding(&text, keys, command).ok_or_else(|| std::io::Error::other("keybindings.json isn't an array"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, new)
}

/// `text` (a keybindings file) with a binding appended to its array, comments kept.
fn with_binding(text: &str, keys: &str, command: Command) -> Option<String> {
    let entry = format!("{{\n        \"key\": \"{keys}\",\n        \"command\": \"{}\"\n    }}", command.id());
    let has_entries = serde_json::from_str::<serde_json::Value>(&theme::strip_jsonc(&text))
        .ok()
        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
        .unwrap_or(false);
    let close = text.rfind(']')?;
    Some(if has_entries {
        // After the last entry's closing brace.
        let last = text[..close].rfind('}').unwrap_or(close);
        format!("{},\n    {entry}{}", &text[..last + 1], &text[last + 1..])
    } else {
        format!("{}\n    {entry}\n{}", text[..close].trim_end(), &text[close..])
    })
}

/// The text for a new keybindings file.
pub const TEMPLATE: &str = "// Place your key bindings in this file to override the defaults\n[\n]\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_key_strings() {
        let (a, b) = parse_keys("cmd+k cmd+s").unwrap();
        assert!(a.cmd && a.key == "k" && b.unwrap().key == "s");
        let (p, _) = parse_keys("ctrl+shift+`").unwrap();
        assert!(p.ctrl && p.shift && !p.cmd && p.key == "`");
        assert_eq!(parse_keys("cmd++").unwrap().0.key, "+");
        assert!(parse_keys("hyper+x").is_none());
        assert_eq!(format_stroke(&parse_keys("shift+cmd+p").unwrap().0), "shift+cmd+p");
    }

    #[test]
    fn appends_bindings_keeping_comments() {
        let once = with_binding(TEMPLATE, "ctrl+m", Command::ToggleMinimap).unwrap();
        assert!(once.starts_with("// Place your key bindings"));
        let twice = with_binding(&once, "cmd+k cmd+m", Command::CommandPalette).unwrap();
        let v: serde_json::Value = serde_json::from_str(&theme::strip_jsonc(&twice)).unwrap();
        assert_eq!(v[0]["key"], "ctrl+m");
        assert_eq!(v[0]["command"], "editor.action.toggleMinimap");
        assert_eq!(v[1]["key"], "cmd+k cmd+m");
    }

    #[test]
    fn user_bindings_override_and_remove() {
        let mut map = defaults();
        apply(&mut map, &json!({ "key": "cmd+k cmd+s", "command": "workbench.action.showCommands" }));
        apply(&mut map, &json!({ "command": "-workbench.action.quickOpen" }));
        let last = map.last().unwrap();
        assert_eq!(last.command, Command::CommandPalette);
        assert!(last.second.is_some());
        assert!(!map.iter().any(|b| b.command == Command::QuickOpen));
    }
}
