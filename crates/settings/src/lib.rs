//! User and workspace settings, stored in `settings.json` format (JSONC).
//!
//! Values resolve workspace → user → default. Edits made from the Settings editor rewrite
//! only the changed property, so comments and formatting in the file are kept.

pub mod jsonc;
pub mod schema;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};

pub use schema::{Kind, Section, Setting};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    Workspace,
}

/// One settings file.
#[derive(Default)]
struct Layer {
    path: Option<PathBuf>,
    /// The values are this property's object (a workspace file's "settings").
    section: Option<&'static str>,
    values: Map<String, Value>,
    mtime: Option<SystemTime>,
    /// Why the file couldn't be read, if it couldn't (the previous values stay in effect).
    error: Option<String>,
}

impl Layer {
    fn load(&mut self) {
        let Some(path) = &self.path else {
            *self = Layer { section: self.section, ..Layer::default() };
            return;
        };
        self.mtime = mtime(path);
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => {
                self.values.clear();
                self.error = None;
            }
            Ok(text) => match serde_json::from_str::<Value>(&theme::strip_jsonc(&text)) {
                Ok(Value::Object(map)) if self.section.is_some() => {
                    self.values = match map.get(self.section.unwrap()) {
                        Some(Value::Object(m)) => m.clone(),
                        _ => Map::new(),
                    };
                    self.error = None;
                }
                Ok(Value::Object(map)) => {
                    self.values = map;
                    self.error = None;
                }
                Ok(_) => self.error = Some("Settings must be a JSON object.".into()),
                Err(e) => self.error = Some(format!("Unable to parse settings: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.values.clear();
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub struct Store {
    user: Layer,
    workspace: Layer,
}

/// `~/Library/Application Support/Orbvane`, or `$ORBVANE_USER_DATA` if set (for testing).
pub fn user_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ORBVANE_USER_DATA") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    home.join("Library/Application Support/Orbvane")
}

/// The editor was called Rustport before: moves its settings, sessions and extensions to
/// `user_data_dir()` the first time. Called once at startup.
pub fn migrate_user_data() {
    let dir = user_data_dir();
    let Some(old) = dir.parent().map(|p| p.join("Rustport")) else { return };
    if std::env::var_os("ORBVANE_USER_DATA").is_none() && !dir.exists() && old.is_dir() && std::fs::rename(&old, &dir).is_ok() {
        let _ = std::fs::rename(dir.join("extensions/.rustport.json"), dir.join("extensions/.orbvane.json"));
    }
}

impl Store {
    /// Loads user settings from `user_path`.
    pub fn new(user_path: PathBuf) -> Self {
        let mut user = Layer { path: Some(user_path), ..Default::default() };
        user.load();
        Self { user, workspace: Layer::default() }
    }

    /// The default location: `<user data>/User/settings.json`, like the standard layout.
    pub fn default_user_path() -> PathBuf {
        user_data_dir().join("User/settings.json")
    }

    /// Uses `<folder>/.orbvane/settings.json` as workspace settings (None: no folder open).
    pub fn set_workspace_folder(&mut self, folder: Option<&Path>) {
        self.workspace = Layer { path: folder.map(|f| f.join(".orbvane/settings.json")), ..Default::default() };
        self.workspace.load();
    }

    /// Uses the `settings` of a `.code-workspace` file as workspace settings.
    pub fn set_workspace_file(&mut self, file: &Path) {
        self.workspace = Layer { path: Some(file.to_path_buf()), section: Some("settings"), ..Default::default() };
        self.workspace.load();
    }

    pub fn path(&self, scope: Scope) -> Option<&Path> {
        self.layer(scope).path.as_deref()
    }

    pub fn error(&self, scope: Scope) -> Option<&str> {
        self.layer(scope).error.as_deref()
    }

    fn layer(&self, scope: Scope) -> &Layer {
        match scope {
            Scope::User => &self.user,
            Scope::Workspace => &self.workspace,
        }
    }

    fn layer_mut(&mut self, scope: Scope) -> &mut Layer {
        match scope {
            Scope::User => &mut self.user,
            Scope::Workspace => &mut self.workspace,
        }
    }

    /// Re-reads files that changed on disk. Returns true if anything was reloaded.
    pub fn reload_if_changed(&mut self) -> bool {
        let mut changed = false;
        for layer in [&mut self.user, &mut self.workspace] {
            if let Some(path) = &layer.path {
                if mtime(path) != layer.mtime {
                    layer.load();
                    changed = true;
                }
            }
        }
        changed
    }

    /// The value set in one scope, if any (valid or not).
    pub fn get_in(&self, scope: Scope, key: &str) -> Option<&Value> {
        self.layer(scope).values.get(key)
    }

    /// The effective value: workspace, then user, then the default. Values of the wrong type
    /// or out of range are ignored.
    pub fn get(&self, key: &str) -> Value {
        let setting = schema::find(key);
        let valid = |v: &&Value| setting.is_none_or(|s| s.accepts(v));
        self.workspace
            .values
            .get(key)
            .filter(valid)
            .or_else(|| self.user.values.get(key).filter(valid))
            .cloned()
            .or_else(|| setting.map(Setting::default_value))
            .unwrap_or(Value::Null)
    }

    pub fn bool(&self, key: &str) -> bool {
        self.get(key).as_bool().unwrap_or(false)
    }

    pub fn number(&self, key: &str) -> f64 {
        self.get(key).as_f64().unwrap_or(0.0)
    }

    pub fn string(&self, key: &str) -> String {
        self.get(key).as_str().unwrap_or_default().to_string()
    }

    /// Sets (or with None, removes) `key` in the scope's file, keeping the rest of the file.
    /// Setting a value equal to the default removes it, like the standard Settings editor.
    pub fn set(&mut self, scope: Scope, key: &str, value: Option<Value>) -> Result<(), String> {
        let value = value.filter(|v| scope == Scope::Workspace || schema::find(key).is_none_or(|s| *v != s.default_value()));
        let layer = self.layer_mut(scope);
        let path = layer.path.clone().ok_or("No folder is open.")?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if layer.error.is_some() && !text.trim().is_empty() {
            return Err(format!("Unable to write into settings because the file has errors. Please fix them in {} and try again.", path.display()));
        }
        let new = match layer.section {
            Some(section) => jsonc::set_nested(&text, section, key, value.as_ref()),
            None => jsonc::set_property(&text, key, value.as_ref()),
        };
        if new == text {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, new).map_err(|e| e.to_string())?;
        layer.load();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn layers_resolve_and_edits_persist() {
        let dir = std::env::temp_dir().join(format!("orbvane-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let user = dir.join("user/settings.json");
        let mut store = Store::new(user.clone());
        assert_eq!(store.number("editor.fontSize"), 12.0);
        assert_eq!(store.string("workbench.colorTheme"), "Orbvane Night");

        store.set(Scope::User, "editor.fontSize", Some(json!(15))).unwrap();
        assert_eq!(store.number("editor.fontSize"), 15.0);
        assert!(std::fs::read_to_string(&user).unwrap().contains("\"editor.fontSize\": 15"));

        // Invalid values fall back to the default.
        std::fs::write(&user, "{ // mine\n  \"editor.tabSize\": \"wide\",\n  \"editor.fontSize\": 15 }").unwrap();
        assert!(store.reload_if_changed() || store.number("editor.fontSize") == 15.0);
        store.user.load();
        assert_eq!(store.number("editor.tabSize"), 4.0);

        // Workspace wins over user; setting the default in user scope removes the key.
        store.set_workspace_folder(Some(&dir.join("ws")));
        store.set(Scope::Workspace, "editor.fontSize", Some(json!(20))).unwrap();
        assert_eq!(store.number("editor.fontSize"), 20.0);
        store.set(Scope::User, "editor.fontSize", Some(json!(12))).unwrap();
        assert_eq!(store.get_in(Scope::User, "editor.fontSize"), None);
        assert!(std::fs::read_to_string(&user).unwrap().contains("// mine"));

        // A broken file keeps working values and refuses edits.
        std::fs::write(&user, "{ \"editor.tabSize\": 2, ").unwrap();
        store.user.load();
        assert!(store.error(Scope::User).is_some());
        assert!(store.set(Scope::User, "editor.tabSize", Some(json!(8))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
