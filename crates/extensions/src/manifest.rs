//! An extension's `package.json`, the standard manifest format: identity, `activationEvents`, the
//! program to run (`main`) and `contributes`. Open VSX extensions whose code is JavaScript still
//! contribute their themes, snippets, languages, keybindings, settings and JSON schemas.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Clone, Debug)]
pub struct Extension {
    /// `publisher.name`, lowercase (identifiers are case-insensitive).
    pub id: String,
    pub publisher: String,
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub description: String,
    /// The extension's folder.
    pub path: PathBuf,
    pub manifest: Value,
    /// Installed from a folder that stays where it is (Install Extension from Location).
    pub linked: bool,
}

/// A command from `contributes.commands`.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandDef {
    pub command: String,
    pub title: String,
    pub category: Option<String>,
    /// An icon name (`$(refresh)`) or an image path in the extension (the dark theme's), shown
    /// where the command is a button (view title bars, inline tree item actions).
    pub icon: Option<String>,
}

/// A view container in the activity bar (`contributes.viewsContainers.activitybar`).
#[derive(Clone, Debug, PartialEq)]
pub struct ViewContainerDef {
    pub id: String,
    pub title: String,
    /// An image path in the extension (SVG or PNG) or an icon name (`$(name)`).
    pub icon: Option<String>,
}

/// A tree view (`contributes.views`): in `container` (`explorer`, or an activity bar
/// container's id).
#[derive(Clone, Debug, PartialEq)]
pub struct ViewDef {
    pub id: String,
    pub name: String,
    pub container: String,
    pub when: Option<String>,
}

/// An entry of a `contributes.menus` menu (`view/title`, `view/item/context`).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItemDef {
    pub command: String,
    pub when: Option<String>,
    /// `navigation` (title bar buttons) and `inline` (item buttons) are drawn as icons; the
    /// rest go in "..." or the context menu.
    pub group: Option<String>,
}

/// A binding from `contributes.keybindings` (`mac` wins over `key` on macOS).
#[derive(Clone, Debug, PartialEq)]
pub struct KeybindingDef {
    pub key: String,
    pub command: String,
    pub when: Option<String>,
    pub args: Value,
}

/// A color theme from `contributes.themes`.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeDef {
    pub label: String,
    /// `vs`, `vs-dark`, `hc-black` or `hc-light`.
    pub ui_theme: String,
    pub path: PathBuf,
}

/// A setting from `contributes.configuration`.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingDef {
    pub key: String,
    /// The property's JSON schema (`type`, `default`, `enum`, `description`...).
    pub schema: Value,
    /// The configuration block's title ("Word Count"), its section in the Settings editor.
    pub section: String,
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// We allow `%key%` placeholders, filled from `package.nls.json`.
fn localize(s: &str, nls: &Value) -> String {
    match s.strip_prefix('%').and_then(|k| k.strip_suffix('%')) {
        Some(key) => match &nls[key] {
            Value::String(t) => t.clone(),
            Value::Object(o) => o.get("message").and_then(Value::as_str).unwrap_or(s).to_string(),
            _ => s.to_string(),
        },
        None => s.to_string(),
    }
}

/// Replaces `%key%` strings anywhere in `v`.
fn localize_all(v: &mut Value, nls: &Value) {
    match v {
        Value::String(s) if s.starts_with('%') => *s = localize(s, nls),
        Value::Array(a) => a.iter_mut().for_each(|x| localize_all(x, nls)),
        Value::Object(o) => o.values_mut().for_each(|x| localize_all(x, nls)),
        _ => {}
    }
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&theme::strip_jsonc(&text)).map_err(|e| format!("{}: {e}", path.display()))
}

/// `arm64` or `x64`, as platforms are named (`darwin-arm64`).
pub fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" }
}

impl Extension {
    /// Reads the extension in `dir` (its `package.json`).
    pub fn load(dir: &Path) -> Result<Extension, String> {
        let mut manifest = read_json(&dir.join("package.json"))?;
        if !manifest.is_object() {
            return Err(format!("{}: package.json should be an object", dir.display()));
        }
        if let Ok(nls) = read_json(&dir.join("package.nls.json")) {
            localize_all(&mut manifest, &nls);
        }
        Extension::from_manifest(manifest, dir)
    }

    /// An extension described by `manifest`, in `dir` (a marketplace extension's manifest
    /// describes one that isn't installed).
    pub fn from_manifest(manifest: Value, dir: &Path) -> Result<Extension, String> {
        let name = text(&manifest, "name");
        if name.is_empty() {
            return Err(format!("{}: package.json has no \"name\"", dir.display()));
        }
        let publisher = match text(&manifest, "publisher") {
            p if p.is_empty() => "undefined_publisher".to_string(),
            p => p,
        };
        let display_name = match text(&manifest, "displayName") {
            d if d.is_empty() => name.clone(),
            d => d,
        };
        Ok(Extension {
            id: format!("{publisher}.{name}").to_lowercase(),
            version: text(&manifest, "version"),
            description: text(&manifest, "description"),
            publisher,
            name,
            display_name,
            path: dir.to_path_buf(),
            manifest,
            linked: false,
        })
    }

    fn contributes(&self, key: &str) -> &Value {
        &self.manifest["contributes"][key]
    }

    fn contributed(&self, key: &str) -> &[Value] {
        self.contributes(key).as_array().map_or(&[], Vec::as_slice)
    }

    /// A path in the extension (`./themes/x.json`).
    pub fn file(&self, rel: &str) -> PathBuf {
        self.path.join(rel.trim_start_matches("./"))
    }

    /// `${extensionPath}`, `${arch}` and `${platform}` in a manifest string.
    pub fn expand(&self, s: &str) -> String {
        s.replace("${extensionPath}", &self.path.to_string_lossy())
            .replace("${arch}", arch())
            .replace("${platform}", &format!("darwin-{}", arch()))
    }

    /// The `main` entry of an Open VSX extension is JavaScript, which we can't run.
    pub fn has_js_code(&self) -> bool {
        // `main` may leave out the `.js` (`./out/extension`).
        let js = |m: &str| match Path::new(m).extension().and_then(|e| e.to_str()) {
            Some("js" | "cjs" | "mjs") => true,
            None => self.file(&format!("{m}.js")).exists(),
            _ => false,
        };
        self.manifest["orbvane"]["main"].is_null() && (self.manifest["main"].as_str().is_some_and(js) || self.manifest["browser"].is_string())
    }

    /// The extension's program: `orbvane.main`, else `main` when it isn't JavaScript.
    pub fn program(&self) -> Option<PathBuf> {
        let main = self.manifest["orbvane"]["main"].as_str().or_else(|| self.manifest["main"].as_str().filter(|_| !self.has_js_code()))?;
        let path = PathBuf::from(self.expand(main));
        Some(if path.is_absolute() { path } else { self.file(&path.to_string_lossy()) })
    }

    /// When the program starts: the manifest's `activationEvents`, plus `onCommand:` for each
    /// contributed command and `onLanguage:` for each contributed language.
    pub fn activation_events(&self) -> Vec<String> {
        let mut events: Vec<String> = self.manifest["activationEvents"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        for c in self.commands() {
            events.push(format!("onCommand:{}", c.command));
        }
        for l in self.contributed("languages") {
            if let Some(id) = l["id"].as_str() {
                events.push(format!("onLanguage:{id}"));
            }
        }
        for v in self.views() {
            events.push(format!("onView:{}", v.id));
        }
        let mut seen = std::collections::HashSet::new();
        events.retain(|e| seen.insert(e.clone()));
        events
    }

    pub fn commands(&self) -> Vec<CommandDef> {
        self.contributed("commands")
            .iter()
            .filter_map(|c| {
                Some(CommandDef {
                    command: c["command"].as_str()?.to_string(),
                    title: c["title"].as_str().map_or_else(|| c["command"].as_str().unwrap_or("").to_string(), String::from),
                    category: c["category"].as_str().map(String::from),
                    icon: match &c["icon"] {
                        Value::String(s) => Some(s.clone()),
                        v => v["dark"].as_str().map(String::from),
                    },
                })
            })
            .collect()
    }

    pub fn view_containers(&self) -> Vec<ViewContainerDef> {
        self.contributes("viewsContainers")["activitybar"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some(ViewContainerDef { id: c["id"].as_str()?.to_string(), title: c["title"].as_str().unwrap_or("").to_string(), icon: c["icon"].as_str().map(String::from) }))
            .collect()
    }

    /// The tree views, in the order the manifest lists them (webviews aren't supported).
    pub fn views(&self) -> Vec<ViewDef> {
        let mut out = Vec::new();
        for (container, views) in self.contributes("views").as_object().into_iter().flatten() {
            for v in views.as_array().into_iter().flatten() {
                if v["type"].as_str() == Some("webview") {
                    continue;
                }
                let Some(id) = v["id"].as_str() else { continue };
                out.push(ViewDef { id: id.to_string(), name: v["name"].as_str().unwrap_or(id).to_string(), container: container.clone(), when: v["when"].as_str().map(String::from) });
            }
        }
        out
    }

    /// The entries of menu `menu` (`view/title`).
    pub fn menu(&self, menu: &str) -> Vec<MenuItemDef> {
        self.contributes("menus")[menu]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some(MenuItemDef { command: m["command"].as_str()?.to_string(), when: m["when"].as_str().map(String::from), group: m["group"].as_str().map(String::from) }))
            .collect()
    }

    pub fn keybindings(&self) -> Vec<KeybindingDef> {
        let list: Vec<Value> = match self.contributes("keybindings") {
            Value::Array(a) => a.clone(),
            o @ Value::Object(_) => vec![o.clone()],
            _ => Vec::new(),
        };
        list.iter()
            .filter_map(|b| {
                Some(KeybindingDef {
                    key: b["mac"].as_str().or_else(|| b["key"].as_str())?.to_string(),
                    command: b["command"].as_str()?.to_string(),
                    when: b["when"].as_str().map(String::from),
                    args: b["args"].clone(),
                })
            })
            .collect()
    }

    pub fn themes(&self) -> Vec<ThemeDef> {
        self.contributed("themes")
            .iter()
            .filter_map(|t| {
                let path = self.file(t["path"].as_str()?);
                let label = t["label"].as_str().or_else(|| t["id"].as_str()).map(String::from).unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                Some(ThemeDef { label, ui_theme: t["uiTheme"].as_str().unwrap_or("vs-dark").to_string(), path })
            })
            .collect()
    }

    /// `(language id, snippets file)`; a snippets file without a language is global.
    pub fn snippets(&self) -> Vec<(Option<String>, PathBuf)> {
        self.contributed("snippets").iter().filter_map(|s| Some((s["language"].as_str().map(String::from), self.file(s["path"].as_str()?)))).collect()
    }

    /// The contributed languages as `languages.json` entries: the fields (`aliases`,
    /// `extensions`, `filenames`, and comments from the `configuration` file) plus ours
    /// (`grammar`, `languageServer`, whose `command` may be a path in the extension).
    pub fn languages(&self) -> Vec<Value> {
        self.contributed("languages")
            .iter()
            .filter_map(|l| {
                let mut e = Map::new();
                e.insert("id".into(), l["id"].as_str()?.into());
                let aliases: Vec<&str> = l["aliases"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if let Some(name) = aliases.first() {
                    e.insert("name".into(), (*name).into());
                    e.insert("aliases".into(), aliases.iter().skip(1).map(|a| Value::from(*a)).collect());
                }
                for key in ["extensions", "filenames", "grammar"] {
                    if !l[key].is_null() {
                        e.insert(key.into(), l[key].clone());
                    }
                }
                if let Some(config) = l["configuration"].as_str().and_then(|p| read_json(&self.file(p)).ok()) {
                    if let Some(line) = config["comments"]["lineComment"].as_str() {
                        e.insert("lineComment".into(), line.into());
                    }
                    if config["comments"]["blockComment"].as_array().is_some_and(|b| b.len() == 2) {
                        e.insert("blockComment".into(), config["comments"]["blockComment"].clone());
                    }
                }
                if let Value::Object(server) = &l["languageServer"] {
                    let mut server = server.clone();
                    if let Some(cmd) = server.get("command").and_then(Value::as_str) {
                        let cmd = self.expand(cmd);
                        let cmd = if cmd.starts_with("./") || cmd.starts_with("../") { self.file(&cmd).to_string_lossy().into_owned() } else { cmd };
                        server.insert("command".into(), cmd.into());
                    }
                    if let Some(args) = server.get("args").and_then(Value::as_array) {
                        let args: Vec<Value> = args.iter().map(|a| a.as_str().map_or(a.clone(), |s| self.expand(s).into())).collect();
                        server.insert("args".into(), args.into());
                    }
                    e.insert("languageServer".into(), server.into());
                }
                Some(Value::Object(e))
            })
            .collect()
    }

    /// The settings of `contributes.configuration` (an object or a list of them).
    pub fn settings(&self) -> Vec<SettingDef> {
        let blocks: Vec<&Value> = match self.contributes("configuration") {
            Value::Array(a) => a.iter().collect(),
            o @ Value::Object(_) => vec![o],
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for block in blocks {
            let section = block["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(&self.display_name).to_string();
            let Some(props) = block["properties"].as_object() else { continue };
            let mut props: Vec<(&String, &Value)> = props.iter().collect();
            // `order` first, then as written (sorts the rest by key; the manifest order reads better).
            props.sort_by_key(|(_, p)| p["order"].as_i64().unwrap_or(i64::MAX));
            for (key, schema) in props {
                out.push(SettingDef { key: key.clone(), schema: schema.clone(), section: section.clone() });
            }
        }
        out
    }

    /// `contributes.jsonValidation`: (file patterns, schema). A relative schema `url` is a file
    /// in the extension, returned as its path.
    pub fn json_validation(&self) -> Vec<(Vec<String>, String)> {
        self.contributed("jsonValidation")
            .iter()
            .filter_map(|j| {
                let patterns = match &j["fileMatch"] {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
                    _ => return None,
                };
                let url = j["url"].as_str()?;
                let url = if url.contains("://") { url.to_string() } else { self.file(url).to_string_lossy().into_owned() };
                Some((patterns, url))
            })
            .collect()
    }

    /// The README, shown on the extension's page.
    pub fn readme(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(&self.path).ok()?;
        entries.flatten().map(|e| e.path()).find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("readme.md")))
    }

    pub fn changelog(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(&self.path).ok()?;
        entries.flatten().map(|e| e.path()).find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("changelog.md")))
    }

    /// `categories` ("Themes", "Snippets"...).
    pub fn categories(&self) -> Vec<String> {
        self.manifest["categories"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
    }

    /// The repository's web address, from `repository` (a string or `{ "url": ... }`).
    pub fn repository(&self) -> Option<String> {
        let r = self.manifest["repository"].as_str().or_else(|| self.manifest["repository"]["url"].as_str())?;
        Some(r.trim_start_matches("git+").trim_end_matches(".git").to_string())
    }

    /// What it contributes, for the extension's page: ("Commands", 3)...
    pub fn contribution_counts(&self) -> Vec<(&'static str, usize)> {
        let counts = [
            ("Commands", self.commands().len()),
            ("Keyboard Shortcuts", self.keybindings().len()),
            ("Color Themes", self.themes().len()),
            ("Snippets", self.snippets().len()),
            ("Programming Languages", self.contributed("languages").len()),
            ("Settings", self.settings().len()),
            ("JSON Validation", self.json_validation().len()),
            ("Views", self.views().len()),
        ];
        counts.into_iter().filter(|(_, n)| *n > 0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn reads_a_manifest_and_its_contributions() {
        let dir = std::env::temp_dir().join(format!("orbvane-ext-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            &[
                (
                    "package.json",
                    r#"{
                        // comments are fine
                        "name": "word-count", "publisher": "Orbvane", "version": "0.1.0", "displayName": "%name%",
                        "main": "bin/${arch}/word-count",
                        "activationEvents": ["onStartupFinished"],
                        "contributes": {
                            "commands": [{ "command": "wordCount.show", "title": "Show Word Count", "category": "Word Count" }],
                            "keybindings": [{ "command": "wordCount.show", "key": "ctrl+alt+w", "mac": "cmd+alt+w" }],
                            "themes": [{ "label": "Quiet", "uiTheme": "vs", "path": "./themes/quiet.json" }],
                            "snippets": [{ "language": "markdown", "path": "./snippets.json" }],
                            "languages": [{ "id": "wc", "aliases": ["Word Count", "wc"], "extensions": [".wc"], "configuration": "./lang.json",
                                            "languageServer": { "command": "./bin/server", "args": ["--path", "${extensionPath}"] } }],
                            "configuration": { "title": "Word Count", "properties": {
                                "wordCount.enabled": { "type": "boolean", "default": true, "description": "Count words.", "order": 1 },
                                "wordCount.mode": { "type": "string", "enum": ["words", "chars"], "default": "words", "order": 0 } } },
                            "jsonValidation": [{ "fileMatch": "wc.json", "url": "./schema.json" }]
                        }
                    }"#,
                ),
                ("package.nls.json", r#"{ "name": "Word Count" }"#),
                ("lang.json", r##"{ "comments": { "lineComment": "#", "blockComment": ["<!--", "-->"] } }"##),
                ("README.md", "# Word Count"),
            ],
        );
        let e = Extension::load(&dir).unwrap();
        assert_eq!((e.id.as_str(), e.display_name.as_str(), e.version.as_str()), ("orbvane.word-count", "Word Count", "0.1.0"));
        assert!(!e.has_js_code());
        assert_eq!(e.program(), Some(dir.join(format!("bin/{}/word-count", arch()))));
        assert_eq!(e.activation_events(), ["onStartupFinished", "onCommand:wordCount.show", "onLanguage:wc"]);
        assert_eq!(e.keybindings()[0].key, "cmd+alt+w");
        assert_eq!(e.themes()[0].path, dir.join("themes/quiet.json"));
        assert_eq!(e.snippets(), [(Some("markdown".to_string()), dir.join("snippets.json"))]);
        let lang = &e.languages()[0];
        assert_eq!(lang["name"], "Word Count");
        assert_eq!(lang["lineComment"], "#");
        assert_eq!(lang["languageServer"]["command"], dir.join("bin/server").to_string_lossy().as_ref());
        assert_eq!(lang["languageServer"]["args"][1], dir.to_string_lossy().as_ref());
        let settings = e.settings();
        assert_eq!(settings.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(), ["wordCount.mode", "wordCount.enabled"]);
        assert_eq!(settings[0].section, "Word Count");
        assert_eq!(e.json_validation(), [(vec!["wc.json".to_string()], dir.join("schema.json").to_string_lossy().into_owned())]);
        assert_eq!(e.readme(), Some(dir.join("README.md")));

        // An Open VSX extension: JavaScript code we don't run.
        write(&dir, &[("package.json", r#"{ "name": "x", "publisher": "p", "main": "./out/extension" }"#), ("out/extension.js", "")]);
        let e = Extension::load(&dir).unwrap();
        assert!(e.has_js_code());
        assert_eq!(e.program(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
