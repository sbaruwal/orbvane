//! The user's `languages.json` (`<user data>/User/languages.json`): changes to the built-in
//! languages and new ones (file patterns, comments, grammar, language server), read once at
//! startup. See `language::init` for the format.

use std::path::PathBuf;

pub fn path() -> PathBuf {
    settings::user_data_dir().join("User").join("languages.json")
}

/// Loads the languages. Returns a message for the user when the file has problems (the
/// languages are loaded anyway, with what could be read).
pub fn load() -> Result<(), String> {
    let mut problems = Vec::new();
    let user = match std::fs::read_to_string(path()) {
        Err(_) => None,
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&theme::strip_jsonc(&text)) {
            Ok(v) => Some(v),
            Err(e) => {
                problems.push(e.to_string());
                None
            }
        },
    };
    // Extensions' languages first, so the user's file can change them.
    let mut entries = crate::contributions::languages();
    match user {
        Some(serde_json::Value::Array(list)) => entries.extend(list),
        Some(serde_json::Value::Object(mut o)) if o.get("languages").is_some_and(|l| l.is_array()) => {
            if let Some(serde_json::Value::Array(list)) = o.remove("languages") {
                entries.extend(list);
            }
        }
        Some(_) => problems.push("languages.json should be a list of languages".into()),
        None => {}
    }
    problems.extend(language::init(Some(&serde_json::Value::Array(entries))));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("languages.json: {}", problems.join("; ")))
    }
}
