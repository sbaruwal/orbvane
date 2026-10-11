//! Debug adapters described as data: how an adapter starts, what `type` launch configurations
//! use for it, the files it debugs, its launch configuration snippet and how to install it.
//! The built-in descriptions are `debuggers.json`; the `debug.adapters` setting adds more or
//! replaces one by its `type`. (lldb-dap, debugpy, Delve and Node need code of their own and
//! stay in `workbench/debug.rs`.)

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// How an adapter is reached.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Start {
    /// Over its stdin and stdout.
    #[default]
    Stdio,
    /// It connects back to us: `{addr}` in its arguments is where we listen.
    ClientAddr,
    /// It listens: `{port}` in its arguments is a free port, which we connect to.
    Server,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Adapter {
    /// What launch configurations' `"type"` says.
    #[serde(rename = "type")]
    pub ty: String,
    /// Other `type`s that mean this adapter.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Its name in the "Select debugger" picker.
    pub label: String,
    /// The program: a name looked up on the PATH, or a path.
    pub command: String,
    /// Its arguments: `{addr}`/`{port}` as `start` says, and `{name}` for the launch
    /// configuration's field `name` (like `{program}`).
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub start: Start,
    /// The request sent instead of the configuration's (an adapter that launches the program
    /// itself is attached to).
    #[serde(default)]
    pub request: Option<String>,
    /// Language ids and file patterns it debugs (listed first for such a file).
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    /// What to do when `command` isn't found.
    #[serde(default)]
    pub install: Option<String>,
    /// Launch configurations for a new `launch.json`.
    #[serde(default)]
    pub configurations: Vec<Value>,
}

impl Adapter {
    /// Whether launch configurations of `ty` are this adapter's.
    pub fn handles(&self, ty: &str) -> bool {
        self.ty == ty || self.aliases.iter().any(|a| a == ty)
    }

    /// Whether it debugs the file at `path` (in language `lang`).
    pub fn debugs(&self, path: &Path, lang: &str) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        self.languages.iter().any(|l| l == lang) || self.files.iter().any(|g| search::GlobSet::parse(g).is_ok_and(|set| set.matches(name)))
    }

    /// The arguments with the configuration's fields filled in (`{addr}` and `{port}` stay
    /// for the client to fill).
    pub fn args_for(&self, config: &Value) -> Vec<String> {
        self.args.iter().map(|a| fill(a, config)).collect()
    }
}

/// `{name}` in `arg` → the configuration's field `name`.
fn fill(arg: &str, config: &Value) -> String {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open..].find('}') else { break };
        out.push_str(&rest[..open]);
        let key = &rest[open + 1..open + len];
        match &config[key] {
            _ if key == "addr" || key == "port" => out.push_str(&rest[open..=open + len]),
            Value::String(s) => out.push_str(s),
            Value::Null => {}
            v => out.push_str(&v.to_string()),
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The built-in descriptions.
pub fn builtin() -> Vec<Adapter> {
    serde_json::from_str(include_str!("debuggers.json")).expect("debuggers.json is valid")
}

/// The built-in descriptions with the `debug.adapters` setting's over them (same `type`
/// replaces; malformed entries are skipped).
pub fn all(setting: &Value) -> Vec<Adapter> {
    let mut list = builtin();
    for v in setting.as_array().into_iter().flatten() {
        let Ok(a) = serde_json::from_value::<Adapter>(v.clone()) else { continue };
        list.retain(|b| b.ty != a.ty);
        list.push(a);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtins_parse_and_fill_arguments() {
        let list = builtin();
        let ruby = list.iter().find(|a| a.handles("rdbg")).unwrap();
        assert_eq!(ruby.start, Start::Server);
        let args = ruby.args_for(&json!({ "program": "/w/app.rb" }));
        assert_eq!(args.join(" "), "--open --host 127.0.0.1 --port {port} --command -- ruby /w/app.rb");
        assert!(list.iter().any(|a| a.debugs(Path::new("/w/lib/main.dart"), "plaintext")));
        assert!(list.iter().find(|a| a.ty == "coreclr").unwrap().debugs(Path::new("x.cs"), ""));
        for a in &list {
            assert_eq!(a.configurations[0]["type"], a.ty.as_str());
        }
    }

    #[test]
    fn the_setting_adds_and_replaces() {
        let setting = json!([
            { "type": "dart", "label": "My Dart", "command": "/opt/dart/bin/dart", "args": ["debug_adapter"] },
            { "type": "zig", "label": "Zig", "command": "zig-dap", "files": ["*.zig"] },
            { "label": "no type: skipped" }
        ]);
        let list = all(&setting);
        assert_eq!(list.iter().filter(|a| a.ty == "dart").count(), 1);
        assert_eq!(list.iter().find(|a| a.ty == "dart").unwrap().command, "/opt/dart/bin/dart");
        assert!(list.iter().any(|a| a.ty == "zig" && a.start == Start::Stdio));
    }
}
