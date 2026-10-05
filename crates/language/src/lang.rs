//! Language definitions: which files a language covers, its comment tokens, the keywords the
//! line lexer colors, its tree-sitter grammar and its language server. The built-in ones come
//! from `languages.json`; the user's own `languages.json` (passed to `init`) changes them or adds
//! languages, so a language with a server needs no code.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde_json::Value;

/// A language: an index into the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lang(u16);

/// The built-in languages code refers to by name (the first entries of `languages.json`).
#[allow(non_upper_case_globals)]
impl Lang {
    pub const Rust: Lang = Lang(0);
    pub const Python: Lang = Lang(1);
    pub const Go: Lang = Lang(2);
    pub const C: Lang = Lang(3);
    pub const Cpp: Lang = Lang(4);
    pub const JavaScript: Lang = Lang(5);
    pub const TypeScript: Lang = Lang(6);
    pub const Toml: Lang = Lang(7);
    pub const Json: Lang = Lang(8);
    pub const Markdown: Lang = Lang(9);
    pub const Shell: Lang = Lang(10);
    /// A Search Editor's results.
    pub const SearchResult: Lang = Lang(11);
    pub const PlainText: Lang = Lang(12);
}

/// How to start a language's server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerDef {
    pub command: &'static str,
    pub args: Vec<&'static str>,
    /// Sent as `initializationOptions` (Null: none).
    pub initialization_options: Value,
    /// Answers to `workspace/configuration`, by section (`{"yaml": {...}}`).
    pub settings: Value,
    /// A shell command that installs the server, offered when it isn't found.
    pub install: Option<&'static str>,
    /// Takes long to load a project (and holds a lot of memory doing it): kept running when
    /// idle unless `languageServers.stopWhenIdle` is "all".
    pub heavy: bool,
}

#[derive(Clone, Debug)]
pub struct LangDef {
    /// The language identifier ("rust", "shellscript"...), used by LSP and snippet files.
    pub id: &'static str,
    pub name: &'static str,
    pub aliases: Vec<&'static str>,
    /// Lowercase, without the dot ("rs", "d.ts").
    pub extensions: Vec<&'static str>,
    pub filenames: Vec<&'static str>,
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    pub(crate) keywords: Vec<&'static str>,
    pub(crate) control: Vec<&'static str>,
    pub(crate) constants: Vec<&'static str>,
    /// `#` starts a comment, not a word.
    pub(crate) hash_comment: bool,
    pub(crate) triple_strings: bool,
    /// `name!` is a macro call.
    pub(crate) macros: bool,
    pub(crate) lifetimes: bool,
    /// A built-in tree-sitter grammar ("rust", "tsx"...).
    pub grammar: Option<&'static str>,
    pub server: Option<ServerDef>,
}

struct Registry {
    defs: Vec<LangDef>,
    /// Exact file names, then extensions (later definitions win).
    filenames: HashMap<&'static str, Lang>,
    extensions: HashMap<&'static str, Lang>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

const BUILTIN: &str = include_str!("../languages.json");

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn strings(v: &Value) -> Option<Vec<&'static str>> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(leak).collect())
}

fn empty(id: &str) -> LangDef {
    LangDef {
        id: leak(id),
        name: leak(id),
        aliases: Vec::new(),
        extensions: Vec::new(),
        filenames: Vec::new(),
        line_comment: None,
        block_comment: None,
        keywords: Vec::new(),
        control: Vec::new(),
        constants: Vec::new(),
        hash_comment: false,
        triple_strings: false,
        macros: false,
        lifetimes: false,
        grammar: None,
        server: None,
    }
}

/// Applies the fields an entry sets to `def`; returns the extensions and file names it set.
fn apply(def: &mut LangDef, e: &serde_json::Map<String, Value>) -> Result<(), String> {
    let id = def.id;
    let text = |k: &str| e.get(k).map(|v| v.as_str().map(leak).ok_or(format!("{id}: \"{k}\" should be a string")));
    let list = |k: &str| e.get(k).map(|v| strings(v).ok_or(format!("{id}: \"{k}\" should be a list of strings")));
    let flag = |k: &str| e.get(k).map(|v| v.as_bool().ok_or(format!("{id}: \"{k}\" should be true or false")));
    if let Some(v) = text("name") {
        def.name = v?;
    }
    if let Some(v) = list("aliases") {
        def.aliases = v?;
    }
    if let Some(v) = list("extensions") {
        def.extensions = v?.into_iter().map(|x| leak(&x.trim_start_matches('.').to_lowercase())).collect();
    }
    if let Some(v) = list("filenames") {
        def.filenames = v?;
    }
    match e.get("lineComment") {
        Some(Value::Null) => def.line_comment = None,
        Some(_) => def.line_comment = Some(text("lineComment").unwrap()?),
        None => {}
    }
    match e.get("blockComment") {
        Some(Value::Null) => def.block_comment = None,
        Some(v) => match strings(v).as_deref() {
            Some([a, b]) => def.block_comment = Some((a, b)),
            _ => return Err(format!("{id}: \"blockComment\" should be [\"start\", \"end\"]")),
        },
        None => {}
    }
    if let Some(v) = list("keywords") {
        def.keywords = v?;
    }
    if let Some(v) = list("controlKeywords") {
        def.control = v?;
    }
    if let Some(v) = list("constants") {
        def.constants = v?;
    }
    if let Some(v) = flag("tripleQuotedStrings") {
        def.triple_strings = v?;
    }
    if let Some(v) = flag("macros") {
        def.macros = v?;
    }
    if let Some(v) = flag("lifetimes") {
        def.lifetimes = v?;
    }
    match e.get("grammar") {
        Some(Value::Null) => def.grammar = None,
        Some(_) => def.grammar = Some(text("grammar").unwrap()?),
        None => {}
    }
    match e.get("languageServer") {
        Some(Value::Null) => def.server = None,
        Some(Value::Object(s)) => {
            let command = s.get("command").and_then(Value::as_str).ok_or(format!("{id}: \"languageServer\" needs a \"command\""))?;
            def.server = Some(ServerDef {
                command: leak(command),
                args: s.get("args").and_then(strings).unwrap_or_default(),
                initialization_options: s.get("initializationOptions").cloned().unwrap_or(Value::Null),
                settings: s.get("settings").cloned().unwrap_or(Value::Null),
                install: s.get("install").and_then(Value::as_str).map(leak),
                heavy: s.get("heavy").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        Some(_) => return Err(format!("{id}: \"languageServer\" should be an object or null")),
        None => {}
    }
    def.hash_comment = def.line_comment == Some("#");
    Ok(())
}

/// The entries of a `languages.json`: a list, or `{"languages": [...]}`.
fn entries(v: &Value) -> Option<&Vec<Value>> {
    v.as_array().or_else(|| v.get("languages").and_then(Value::as_array))
}

fn build(user: Option<&Value>) -> (Registry, Vec<String>) {
    let mut errors = Vec::new();
    let builtin: Value = serde_json::from_str(BUILTIN).expect("languages.json");
    let mut defs: Vec<LangDef> = Vec::new();
    let mut filenames = HashMap::new();
    let mut extensions = HashMap::new();
    let mut add = |entry: &Value, defs: &mut Vec<LangDef>, errors: &mut Vec<String>| {
        let Some(e) = entry.as_object() else { return errors.push("a language should be an object".into()) };
        let Some(id) = e.get("id").and_then(Value::as_str) else { return errors.push("a language needs an \"id\"".into()) };
        let i = match defs.iter().position(|d| d.id == id) {
            Some(i) => i,
            None => {
                defs.push(empty(id));
                defs.len() - 1
            }
        };
        if let Err(err) = apply(&mut defs[i], e) {
            errors.push(err);
        }
        let lang = Lang(i as u16);
        // The files this entry claims are its own now, even if another language had them.
        if e.contains_key("extensions") {
            for x in &defs[i].extensions {
                extensions.insert(*x, lang);
            }
        }
        if e.contains_key("filenames") {
            for f in &defs[i].filenames {
                filenames.insert(*f, lang);
            }
        }
    };
    for entry in entries(&builtin).into_iter().flatten() {
        add(entry, &mut defs, &mut errors);
    }
    if let Some(user) = user {
        match entries(user) {
            Some(list) => {
                for entry in list {
                    add(entry, &mut defs, &mut errors);
                }
            }
            None => errors.push("languages.json should be a list of languages".into()),
        }
    }
    (Registry { defs, filenames, extensions }, errors)
}

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| build(None).0)
}

/// Loads the languages: the built-in ones changed and extended by the user's `languages.json`
/// (already parsed). Returns problems found in the user's file. Call once, before anything
/// asks about a language; later calls (and calls after a lookup) change nothing.
pub fn init(user: Option<&Value>) -> Vec<String> {
    let (registry, errors) = build(user);
    let _ = REGISTRY.set(registry);
    errors
}

impl Lang {
    /// The language of a file, by name or extension (the longest matching one: `x.d.ts`).
    pub fn detect(path: Option<&Path>) -> Lang {
        let Some(name) = path.and_then(|p| p.file_name()).and_then(|n| n.to_str()) else { return Lang::PlainText };
        let r = registry();
        if let Some(&lang) = r.filenames.get(name) {
            return lang;
        }
        let lower = name.to_lowercase();
        // Every suffix after a dot, longest first (".gitignore" is an extension too).
        lower
            .match_indices('.')
            .filter_map(|(i, _)| r.extensions.get(&lower[i + 1..]).copied())
            .next()
            .unwrap_or(Lang::PlainText)
    }

    /// A language by id, alias or name, ignoring case ("rust", "rs", "C++").
    pub fn from_name(name: &str) -> Option<Lang> {
        let name = name.trim();
        let r = registry();
        let found = r.defs.iter().position(|d| {
            d.id.eq_ignore_ascii_case(name) || d.name.eq_ignore_ascii_case(name) || d.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
        });
        found.map(|i| Lang(i as u16))
    }

    /// All languages, in registry order.
    pub fn all() -> impl Iterator<Item = Lang> {
        (0..registry().defs.len()).map(|i| Lang(i as u16))
    }

    pub fn def(self) -> &'static LangDef {
        let r = registry();
        r.defs.get(self.0 as usize).unwrap_or(&r.defs[Lang::PlainText.0 as usize])
    }

    pub fn name(self) -> &'static str {
        self.def().name
    }

    /// The language identifier ("rust", "shellscript"...), used by LSP and snippet files.
    pub fn id(self) -> &'static str {
        self.def().id
    }

    /// The file extensions and names this language covers, for pickers ("*.rs").
    pub fn patterns(self) -> Vec<String> {
        let d = self.def();
        d.extensions.iter().map(|x| format!(".{x}")).chain(d.filenames.iter().map(|f| f.to_string())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn detect(name: &str) -> &'static str {
        Lang::detect(Some(&PathBuf::from("/x").join(name))).id()
    }

    #[test]
    fn named_languages_match_the_file() {
        let named = [
            (Lang::Rust, "rust"),
            (Lang::Python, "python"),
            (Lang::Go, "go"),
            (Lang::C, "c"),
            (Lang::Cpp, "cpp"),
            (Lang::JavaScript, "javascript"),
            (Lang::TypeScript, "typescript"),
            (Lang::Toml, "toml"),
            (Lang::Json, "json"),
            (Lang::Markdown, "markdown"),
            (Lang::Shell, "shellscript"),
            (Lang::SearchResult, "search-result"),
            (Lang::PlainText, "plaintext"),
        ];
        for (lang, id) in named {
            assert_eq!(lang.id(), id);
        }
    }

    #[test]
    fn detects_by_name_and_extension() {
        assert_eq!(detect("main.rs"), "rust");
        assert_eq!(detect("App.TSX"), "typescriptreact");
        assert_eq!(detect("Cargo.lock"), "toml");
        assert_eq!(detect("Dockerfile"), "dockerfile");
        assert_eq!(detect(".gitignore"), "ignore");
        assert_eq!(detect("tsconfig.json"), "jsonc");
        assert_eq!(detect("data.json"), "json");
        assert_eq!(detect("notes"), "plaintext");
        assert_eq!(Lang::detect(None), Lang::PlainText);
        assert_eq!(Lang::from_name("C++"), Some(Lang::Cpp));
        assert_eq!(Lang::from_name("py"), Some(Lang::Python));
        assert_eq!(Lang::Shell.def().line_comment, Some("#"));
        assert!(Lang::Shell.def().hash_comment);
    }

    #[test]
    fn user_entries_change_and_add_languages() {
        let user: Value = serde_json::from_str(
            r#"{"languages": [
                {"id": "cpp", "extensions": [".h", ".cpp"]},
                {"id": "rust", "languageServer": {"command": "ra-multiplex", "args": ["client"], "initializationOptions": {"check": {"command": "clippy"}}}},
                {"id": "gleam", "name": "Gleam", "extensions": ["gleam"], "lineComment": "//", "languageServer": {"command": "gleam", "args": ["lsp"]}},
                {"id": "go", "languageServer": null},
                {"id": "bad", "blockComment": "/*"}
            ]}"#,
        )
        .unwrap();
        let (r, errors) = build(Some(&user));
        assert_eq!(errors, ["bad: \"blockComment\" should be [\"start\", \"end\"]"]);
        let by_ext = |x: &str| r.defs[r.extensions[x].0 as usize].id;
        assert_eq!(by_ext("h"), "cpp");
        assert_eq!(by_ext("c"), "c");
        assert_eq!(by_ext("gleam"), "gleam");
        let rust = &r.defs[Lang::Rust.0 as usize];
        let server = rust.server.as_ref().unwrap();
        assert_eq!((server.command, server.args.as_slice()), ("ra-multiplex", &["client"][..]));
        assert_eq!(server.initialization_options["check"]["command"], "clippy");
        assert_eq!(rust.keywords.len(), 29, "untouched fields stay");
        assert!(r.defs[Lang::Go.0 as usize].server.is_none());
        assert_eq!(r.defs.last().unwrap().id, "bad");
    }
}
