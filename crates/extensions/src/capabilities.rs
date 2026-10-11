//! What of an extension works in Orbvane, read from its `package.json`. Extensions written for
//! Orbvane (`orbvane.main`, or from Orbvane's registry) work entirely. Others are packages for
//! another editor: their declarative contributions that Orbvane reads work, their JavaScript
//! code doesn't run, and some contributions (TextMate grammars, icon themes) aren't used.

use serde_json::Value;

/// How much of an extension works here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Everything it contributes is used.
    Works,
    /// Some of it is used, some isn't.
    Partly,
    /// Nothing it contributes is used.
    Nothing,
}

impl Verdict {
    /// The badge in the list (short: rows are narrow; the page says more).
    pub fn badge(self) -> &'static str {
        match self {
            Verdict::Works => "Works",
            Verdict::Partly => "Partly",
            Verdict::Nothing => "Won't work",
        }
    }

    /// In words.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Works => "Works",
            Verdict::Partly => "Partly works",
            Verdict::Nothing => "Won't work in Orbvane",
        }
    }
}

/// What works and what doesn't, in words for the badge and the extension's page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub verdict: Verdict,
    /// What's used ("Color themes", "Snippets"...).
    pub works: Vec<&'static str>,
    /// What isn't ("Its JavaScript code", "TextMate grammars"...).
    pub unused: Vec<&'static str>,
}

impl Capabilities {
    /// One sentence for the extension's page.
    pub fn sentence(&self) -> String {
        let list = |items: &[&str]| items.join(", ").to_lowercase();
        match self.verdict {
            Verdict::Works if self.works.is_empty() => "Works in Orbvane.".into(),
            Verdict::Works => format!("Works in Orbvane: {}.", list(&self.works)),
            Verdict::Partly => format!("Works in Orbvane: {}. Not used: {}.", list(&self.works), list(&self.unused)),
            Verdict::Nothing if self.unused.is_empty() => "Contributes nothing Orbvane uses.".into(),
            Verdict::Nothing => format!("Won't work in Orbvane: {} not used.", list(&self.unused)),
        }
    }
}

fn has(manifest: &Value, key: &str) -> bool {
    match &manifest["contributes"][key] {
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

/// Reads an extension's manifest. `native`: written for Orbvane (from its registry).
pub fn of_manifest(manifest: &Value, native: bool) -> Capabilities {
    let native = native || manifest["orbvane"]["main"].is_string();
    let code = !native && (manifest["main"].is_string() || manifest["browser"].is_string());
    let mut works = Vec::new();
    let mut unused = Vec::new();
    if has(manifest, "themes") {
        works.push("Color themes");
    }
    if has(manifest, "snippets") {
        works.push("Snippets");
    }
    if has(manifest, "languages") {
        works.push("Languages");
    }
    if has(manifest, "jsonValidation") {
        works.push("JSON schemas");
    }
    if native {
        // Its program runs: commands, views, settings and shortcuts are all its own.
        for (key, what) in [("commands", "Commands"), ("views", "Views"), ("configuration", "Settings"), ("keybindings", "Keyboard shortcuts")] {
            if has(manifest, key) {
                works.push(what);
            }
        }
        return Capabilities { verdict: Verdict::Works, works, unused };
    }
    // Settings and shortcuts only matter to code that reads them or commands they run.
    if has(manifest, "configuration") && !code {
        works.push("Settings");
    }
    if code {
        unused.push("Its JavaScript code (commands, views, debuggers)");
    }
    if has(manifest, "grammars") {
        unused.push("TextMate grammars");
    }
    if has(manifest, "iconThemes") || has(manifest, "productIconThemes") {
        unused.push("Icon themes");
    }
    let verdict = match (works.is_empty(), unused.is_empty()) {
        (_, true) => Verdict::Works,
        (false, false) => Verdict::Partly,
        (true, false) => Verdict::Nothing,
    };
    Capabilities { verdict, works, unused }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_theme_works() {
        let c = of_manifest(&json!({ "contributes": { "themes": [{ "path": "./t.json" }] } }), false);
        assert_eq!((c.verdict, c.works.clone()), (Verdict::Works, vec!["Color themes"]));
        assert_eq!(c.sentence(), "Works in Orbvane: color themes.");
    }

    #[test]
    fn code_with_snippets_partly_works() {
        let m = json!({ "main": "./out/extension.js", "contributes": { "snippets": [{}], "commands": [{}], "configuration": { "properties": {} } } });
        let c = of_manifest(&m, false);
        assert_eq!(c.verdict, Verdict::Partly);
        assert_eq!(c.works, vec!["Snippets"]);
        assert!(c.sentence().starts_with("Works in Orbvane: snippets. Not used: its javascript code"));
    }

    #[test]
    fn code_alone_wont_work() {
        let m = json!({ "main": "./extension.js", "contributes": { "commands": [{}], "grammars": [{}] } });
        let c = of_manifest(&m, false);
        assert_eq!(c.verdict, Verdict::Nothing);
        assert_eq!(c.unused, vec!["Its JavaScript code (commands, views, debuggers)", "TextMate grammars"]);
    }

    #[test]
    fn orbvane_extensions_work() {
        let m = json!({ "orbvane": { "main": "bin/x" }, "main": "ignored.js", "contributes": { "commands": [{}] } });
        assert_eq!(of_manifest(&m, false).verdict, Verdict::Works);
        assert_eq!(of_manifest(&json!({ "main": "x.js" }), true).verdict, Verdict::Works);
    }
}
