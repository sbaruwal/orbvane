//! JSON schemas for the files that configure the editor (settings, keybindings, launch, tasks,
//! languages and workspace files), handed to the built-in JSON language server, which uses
//! them for completion, hovers and validation.

use serde_json::{json, Map, Value};
use settings::schema::Kind;

use crate::commands::Command;

/// Settings that aren't in the Settings editor (their values are lists or objects).
fn extra_settings() -> Value {
    json!({
        "emmet.excludeLanguages": {
            "type": "array",
            "items": { "type": "string" },
            "default": ["markdown"],
            "description": "An array of languages where Emmet abbreviations should not be expanded."
        }
    })
}

/// The settings file: every known setting with its type, default and description.
pub fn settings(themes: &[String]) -> Value {
    let mut properties = Map::new();
    for s in settings::schema::all() {
        let mut p = json!({ "description": s.description, "default": s.default_value() });
        match s.kind {
            Kind::Bool => p["type"] = json!("boolean"),
            Kind::Number { min, max, integer } => {
                p["type"] = json!(if integer { "integer" } else { "number" });
                if min.is_finite() && min > f64::MIN {
                    p["minimum"] = json!(min);
                }
                if max.is_finite() && max < f64::MAX {
                    p["maximum"] = json!(max);
                }
            }
            Kind::String => p["type"] = json!("string"),
            // "true"/"false" options are JSON booleans.
            Kind::Enum(options) => {
                let value = |o: &str| match o {
                    "true" => json!(true),
                    "false" => json!(false),
                    _ => json!(o),
                };
                p["enum"] = Value::Array(options.iter().map(|(o, _)| value(o)).collect());
                p["enumDescriptions"] = Value::Array(options.iter().map(|(_, d)| json!(d)).collect());
            }
            Kind::Json(schema) => {
                if let Ok(Value::Object(schema)) = serde_json::from_str::<Value>(schema) {
                    p.as_object_mut().unwrap().extend(schema);
                }
            }
            Kind::Theme => {
                p["type"] = json!("string");
                p["enum"] = json!(themes);
                p["errorMessage"] = json!("Unknown color theme.");
            }
        }
        properties.insert(s.key.to_string(), p);
    }
    if let Value::Object(extra) = extra_settings() {
        properties.extend(extra);
    }
    json!({
        "type": "object",
        "allowComments": true,
        "allowTrailingCommas": true,
        "properties": properties,
        "additionalProperties": false,
        "errorMessage": "Unknown Configuration Setting",
    })
}

/// `keybindings.json`: the wording, with every command id offered.
pub fn keybindings() -> Value {
    let ids: Vec<&str> = Command::all().iter().map(|c| c.id()).collect();
    let titles: Vec<&str> = Command::all().iter().map(|c| c.title()).collect();
    json!({
        "type": "array",
        "allowComments": true,
        "allowTrailingCommas": true,
        "items": {
            "type": "object",
            "required": ["key"],
            "defaultSnippets": [{ "body": { "key": "$1", "command": "$2" } }],
            "properties": {
                "key": { "type": "string", "description": "Key or key sequence (separated by space)" },
                "command": {
                    "anyOf": [
                        { "type": "string", "enum": ids, "enumDescriptions": titles },
                        { "type": "string" }
                    ],
                    "description": "Name of the command to execute"
                },
                "when": { "type": "string", "description": "Condition when the key is active." },
                "args": { "description": "Arguments to pass to the command to execute." }
            }
        }
    })
}

pub fn launch() -> Value {
    serde_json::from_str(include_str!("json_schemas/launch.json")).expect("valid launch schema")
}

pub fn tasks() -> Value {
    serde_json::from_str(include_str!("json_schemas/tasks.json")).expect("valid tasks schema")
}

/// The user's `languages.json`.
pub fn languages() -> Value {
    let ids: Vec<&str> = language::Lang::all().map(|l| l.id()).collect();
    let strings = |description: &str| json!({ "type": "array", "items": { "type": "string" }, "description": description });
    let language = json!({
        "type": "object",
        "required": ["id"],
        "defaultSnippets": [{ "label": "New language", "body": { "id": "${1:id}", "name": "${2:Name}", "extensions": [".${3:ext}"] } }],
        "properties": {
            "id": { "anyOf": [{ "enum": ids }, { "type": "string" }], "description": "The language's id. An existing id changes that language; a new one adds a language." },
            "name": { "type": "string", "description": "The name shown in the status bar and the language picker." },
            "aliases": strings("Other names for the language (Markdown code fences, Change Language Mode)."),
            "extensions": strings("File extensions, with the dot (\".rs\")."),
            "filenames": strings("Exact file names (\"Makefile\")."),
            "lineComment": { "type": "string", "description": "The line comment token (Toggle Line Comment)." },
            "blockComment": { "type": "array", "items": { "type": "string" }, "minItems": 2, "maxItems": 2, "description": "The block comment's start and end tokens." },
            "grammar": { "enum": language::GRAMMARS, "description": "The built-in tree-sitter grammar that highlights the language." },
            "keywords": strings("Keywords, for highlighting without a grammar."),
            "controlKeywords": strings("Control flow keywords (`if`, `return`...), highlighted in their own color."),
            "constants": strings("Words highlighted as constants."),
            "tripleQuotedStrings": { "type": "boolean", "description": "Whether `\"\"\"` strings span lines." },
            "macros": { "type": "boolean", "description": "Whether `name!` is a macro (Rust)." },
            "lifetimes": { "type": "boolean", "description": "Whether `'a` is a lifetime (Rust)." },
            "languageServer": {
                "description": "The language server to start for the language; null turns it off.",
                "anyOf": [
                    { "type": "null" },
                    {
                        "type": "object",
                        "required": ["command"],
                        "properties": {
                            "command": { "type": "string", "description": "The server's executable, found on PATH." },
                            "args": strings("Arguments for the server."),
                            "initializationOptions": { "description": "Sent as `initializationOptions` with `initialize`." },
                            "settings": { "type": "object", "description": "The server's settings, answering `workspace/configuration`." },
                            "install": { "type": "string", "description": "A shell command that installs the server, offered when it isn't found." },
                            "heavy": { "type": "boolean", "description": "The server takes long to load a project, so it keeps running when idle unless languageServers.stopWhenIdle is \"all\"." }
                        }
                    }
                ]
            }
        }
    });
    json!({
        "allowComments": true,
        "allowTrailingCommas": true,
        "anyOf": [
            { "type": "object", "properties": { "languages": { "type": "array", "items": { "$ref": "#/definitions/language" } } } },
            { "type": "array", "items": { "$ref": "#/definitions/language" } }
        ],
        "definitions": { "language": language }
    })
}

/// A `.code-workspace` file: folders, and settings, launch and tasks for the workspace.
pub fn workspace(settings: &Value) -> Value {
    let (mut launch, mut tasks) = (launch(), tasks());
    // The embedded schemas' `#/definitions/...` refs resolve against this schema's root.
    let mut definitions = Map::new();
    for s in [&mut launch, &mut tasks] {
        if let Some(Value::Object(d)) = s.as_object_mut().and_then(|o| o.remove("definitions")) {
            definitions.extend(d);
        }
    }
    json!({
        "type": "object",
        "allowComments": true,
        "allowTrailingCommas": true,
        "required": ["folders"],
        "properties": {
            "folders": {
                "type": "array",
                "description": "List of folders to be loaded in the workspace.",
                "items": {
                    "type": "object",
                    "defaultSnippets": [{ "body": { "path": "$1" } }],
                    "properties": {
                        "path": { "type": "string", "description": "A file path. e.g. `/root/folderA` or `./folderA` for a relative path that will be resolved against the location of the workspace file." },
                        "name": { "type": "string", "description": "An optional name for the folder. " },
                        "uri": { "type": "string", "description": "URI of the folder" }
                    }
                }
            },
            "settings": settings,
            "launch": launch,
            "tasks": tasks,
            "extensions": { "type": "object", "description": "Workspace extensions" }
        },
        "errorMessage": "Unknown workspace configuration property",
        "additionalProperties": false,
        "definitions": definitions
    })
}

/// The initialization options of the built-in JSON server: each schema and the files it's for.
/// The `json.schemas` setting's entries as associations: `url` relative to `folder` (the
/// first workspace folder) or absolute becomes a `file://` URI; an entry with its `schema`
/// inline gets a made-up URI.
pub fn from_setting(setting: &Value, folder: Option<&std::path::Path>) -> Vec<Value> {
    let entries = setting.as_array().into_iter().flatten().enumerate();
    entries
        .filter_map(|(i, e)| {
            let file_match: Vec<Value> = match &e["fileMatch"] {
                Value::String(s) => vec![json!(s)],
                Value::Array(a) => a.clone(),
                _ => Vec::new(),
            };
            let uri = match e["url"].as_str() {
                Some(u) if u.contains("://") => u.to_string(),
                Some(u) if u.starts_with('/') => format!("file://{u}"),
                Some(u) => format!("file://{}", folder?.join(u.trim_start_matches("./")).display()),
                None => format!("orbvane://schemas/setting/{i}"),
            };
            let mut entry = json!({ "fileMatch": file_match, "uri": uri });
            if e["schema"].is_object() {
                entry["schema"] = e["schema"].clone();
            } else if e["url"].is_null() {
                return None;
            }
            Some(entry)
        })
        .collect()
}

pub fn associations(themes: &[String]) -> Value {
    let user = settings::user_data_dir().join("User");
    let user_file = |name: &str| user.join(name).to_string_lossy().into_owned();
    let settings = settings(themes);
    let mut schemas = json!([
        { "fileMatch": [user_file("settings.json"), "/.orbvane/settings.json"], "uri": "orbvane://schemas/settings/user", "schema": settings },
        { "fileMatch": [user_file("keybindings.json")], "uri": "orbvane://schemas/keybindings", "schema": keybindings() },
        { "fileMatch": [user_file("languages.json")], "uri": "orbvane://schemas/languages", "schema": languages() },
        { "fileMatch": ["/.orbvane/launch.json"], "uri": "orbvane://schemas/launch", "schema": launch() },
        { "fileMatch": ["/.orbvane/tasks.json"], "uri": "orbvane://schemas/tasks", "schema": tasks() },
        { "fileMatch": ["*.code-workspace"], "uri": "orbvane://schemas/workspaceConfig", "schema": workspace(&settings) },
    ]);
    // Extensions' `jsonValidation`.
    schemas.as_array_mut().unwrap().extend(crate::contributions::json_schemas());
    json!({ "schemas": schemas })
}

#[cfg(test)]
mod tests {
    use super::*;
    use json::parse::Doc;
    use json::schema::Validator;

    fn problems(schema: &Value, text: &str) -> Vec<String> {
        let doc = Doc::parse(text);
        Validator::new(&doc, schema).run().0.into_iter().map(|p| format!("{}: {}", &text[p.start..p.end], p.message)).collect()
    }

    #[test]
    fn schemas_accept_the_templates_and_catch_mistakes() {
        let settings = settings(&["Orbvane Night".to_string(), "Orbvane Day".to_string()]);
        let ok = r#"{ "editor.fontSize": 14, "files.autoSave": "afterDelay", "workbench.colorTheme": "Orbvane Day" }"#;
        assert!(problems(&settings, ok).is_empty(), "{:?}", problems(&settings, ok));
        assert_eq!(
            problems(&settings, r#"{ "editor.fontSize": "big", "editor.nope": 1 }"#),
            ["\"big\": Incorrect type. Expected \"number\".", "\"editor.nope\": Unknown Configuration Setting"]
        );
        // Every default is valid.
        for s in settings::schema::all() {
            let text = format!("{{ {}: {} }}", Value::from(s.key), s.default_value());
            assert!(problems(&settings, &text).is_empty(), "{text}: {:?}", problems(&settings, &text));
        }
        let launch = launch();
        let ok = r#"{ "version": "0.2.0", "configurations": [
            { "type": "lldb-dap", "request": "launch", "name": "Demo", "program": "${workspaceFolder}/prog", "args": [], "cwd": "${workspaceFolder}" },
            { "type": "debugpy", "request": "launch", "name": "Py", "program": "${file}", "console": "internalConsole" } ] }"#;
        assert!(problems(&launch, ok).is_empty(), "{:?}", problems(&launch, ok));
        assert_eq!(problems(&launch, r#"{ "configurations": [{ "type": "debugpy", "request": "go", "name": "x" }] }"#), [
            "\"go\": Value is not accepted. Valid values: \"launch\", \"attach\"."
        ]);
        // New configurations keep the key order, with `name` the first tab stop.
        let text = r#"{ "configurations": [  ] }"#;
        let doc = Doc::parse(text);
        let items = json::features::Analysis::new(&doc, Some(&launch)).complete(text.find("[ ").unwrap() + 1);
        let item = items.iter().find(|i| i.label == "LLDB DAP: Launch").unwrap();
        assert!(item.insert.starts_with("{\n\t\"type\": \"lldb-dap\",\n\t\"request\": \"launch\",\n\t\"name\": \"${1:Launch}\""), "{}", item.insert);
        assert!(item.insert.contains("\"args\": [],"));
        let tasks = tasks();
        let ok = r#"{ "version": "2.0.0", "tasks": [{ "label": "echo", "type": "shell", "command": "echo Hello", "group": { "kind": "build", "isDefault": true } }] }"#;
        assert!(problems(&tasks, ok).is_empty(), "{:?}", problems(&tasks, ok));
        let keys = keybindings();
        assert!(problems(&keys, r#"[{ "key": "cmd+k cmd+m", "command": "-editor.action.toggleMinimap" }]"#).is_empty());
        let langs = languages();
        assert!(problems(&langs, r#"{ "languages": [{ "id": "cpp", "extensions": [".h"] }, { "id": "rust", "languageServer": null }] }"#).is_empty());
        assert_eq!(problems(&langs, r#"[{ "id": "x", "grammar": "cobol" }]"#).len(), 1);
        let ws = workspace(&settings);
        assert!(problems(&ws, r#"{ "folders": [{ "path": "." }], "settings": { "editor.tabSize": 2 }, "tasks": { "version": "2.0.0", "tasks": [] } }"#).is_empty());
    }

    #[test]
    fn schemas_from_the_setting() {
        let setting = json!([
            { "fileMatch": ["*.conf.json"], "url": "./schemas/conf.json" },
            { "fileMatch": "/app/x.json", "url": "https://example.com/x.json" },
            { "fileMatch": ["a.json"], "schema": { "type": "object" } },
            { "fileMatch": ["nothing.json"] },
        ]);
        let got = from_setting(&setting, Some(std::path::Path::new("/w")));
        assert_eq!(
            Value::Array(got),
            json!([
                { "fileMatch": ["*.conf.json"], "uri": "file:///w/schemas/conf.json" },
                { "fileMatch": ["/app/x.json"], "uri": "https://example.com/x.json" },
                { "fileMatch": ["a.json"], "uri": "orbvane://schemas/setting/2", "schema": { "type": "object" } },
            ])
        );
    }
}
