//! Language features from extensions' code: hover, completion, definition, code action and
//! formatting providers (`languages/registerProvider`, asked with `provideHover`,
//! `provideCompletionItems`, `provideDefinition`, `provideCodeActions` and
//! `provideDocumentFormattingEdits`), and diagnostics (`languages/setDiagnostics`). Answers join
//! the language server's: hovers are stacked under it, suggestions and code actions are added
//! to its lists, definitions are asked when there's no server or it found none, and formatters
//! are used for files no server formats. Positions are UTF-8 columns.
//!
//! Extensions' diagnostics live in `diagnostics` and are merged into `Servers::diagnostics` (the
//! one place the editor reads them), tagged with `orbvaneCollection` in `raw` so they can be
//! taken out again; a server publishing replaces its own and they're merged back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp::Encoding;
use serde_json::{json, Value};
use text::Pos;

use super::Workbench;

/// Which documents a provider is for.
#[derive(Clone, Debug)]
enum Filter {
    Language(String),
    /// A glob on the path, optionally with a language.
    Pattern(Option<String>, search::GlobSet),
}

#[derive(Clone, Debug)]
struct Provider {
    ext: String,
    id: String,
    kind: String,
    selector: Vec<Filter>,
    triggers: Vec<String>,
}

#[derive(Default)]
pub(super) struct ExtLanguages {
    providers: Vec<Provider>,
    /// By (extension, collection): each file's diagnostics, in UTF-8 columns.
    pub diagnostics: HashMap<(String, String), HashMap<PathBuf, Vec<lsp::Diagnostic>>>,
    /// Go to Definition asked the language server first; if it finds nothing, ask these.
    definition_fallback: Option<(usize, Pos)>,
}

impl Filter {
    fn parse(v: &Value) -> Option<Filter> {
        match v {
            Value::String(l) => Some(Filter::Language(l.clone())),
            Value::Object(_) => match v["pattern"].as_str() {
                Some(p) => Some(Filter::Pattern(v["language"].as_str().map(String::from), search::GlobSet::parse(p).ok()?)),
                None => v["language"].as_str().map(|l| Filter::Language(l.to_string())),
            },
            _ => None,
        }
    }

    fn matches(&self, lang: &str, path: Option<&Path>) -> bool {
        match self {
            Filter::Language(l) => l == "*" || l == lang,
            Filter::Pattern(l, glob) => {
                l.as_deref().is_none_or(|l| l == lang) && path.is_some_and(|p| glob.matches(&p.to_string_lossy()) || p.file_name().is_some_and(|n| glob.matches(&n.to_string_lossy())))
            }
        }
    }
}

impl Workbench {
    /// `languages/registerProvider`.
    pub(super) fn ext_register_provider(&mut self, ext: &str, params: &Value) {
        let (Some(id), Some(kind)) = (params["id"].as_str(), params["kind"].as_str()) else { return };
        let p = Provider {
            ext: ext.to_string(),
            id: id.to_string(),
            kind: kind.to_string(),
            selector: params["selector"].as_array().into_iter().flatten().filter_map(Filter::parse).collect(),
            triggers: params["triggerCharacters"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(),
        };
        let list = &mut self.ext_languages.providers;
        list.retain(|q| !(q.ext == p.ext && q.id == p.id && q.kind == p.kind));
        list.push(p);
    }

    /// An extension stopped: its providers and diagnostics go.
    pub(super) fn ext_forget_languages(&mut self, ext: &str) {
        self.ext_languages.providers.retain(|p| p.ext != ext);
        let gone: Vec<(String, String)> = self.ext_languages.diagnostics.keys().filter(|(e, _)| e == ext).cloned().collect();
        let mut paths = Vec::new();
        for key in gone {
            if let Some(files) = self.ext_languages.diagnostics.remove(&key) {
                paths.extend(files.into_keys());
            }
        }
        for path in paths {
            self.ext_merge_diagnostics(&path);
        }
    }

    fn ext_providers_for(&self, kind: &str, doc: usize) -> Vec<Provider> {
        let Some(d) = self.docs.get(doc).and_then(Option::as_ref) else { return Vec::new() };
        let (lang, path) = (d.lang.id(), d.buffer.path());
        self.ext_languages.providers.iter().filter(|p| p.kind == kind && p.selector.iter().any(|f| f.matches(lang, path))).cloned().collect()
    }

    /// Whether `typed` is a completion trigger of a provider for the active document.
    pub(super) fn ext_completion_trigger(&self, doc: usize, typed: &str) -> bool {
        self.ext_providers_for("completion", doc).iter().any(|p| p.triggers.iter().any(|t| t == typed))
    }

    pub(super) fn ext_has_completion(&self, doc: usize) -> bool {
        !self.ext_providers_for("completion", doc).is_empty()
    }

    /// Asks each provider of `kind` for document `doc`.
    fn ext_provide(&mut self, kind: &str, method: &str, doc: usize, pos: Pos, extra: Value, waiter: impl Fn() -> super::ext_host::Waiter) -> usize {
        let providers = self.ext_providers_for(kind, doc);
        let Some(d) = self.docs.get(doc).and_then(Option::as_ref) else { return 0 };
        let document = super::ext_host::doc_json(d, true);
        let position = json!({ "line": pos.line, "character": Encoding::Utf8.to_lsp(&d.buffer.line(pos.line), pos.col) });
        for p in &providers {
            let mut params = json!({ "provider": p.id, "document": document, "position": position });
            if let (Value::Object(o), Value::Object(e)) = (&mut params, &extra) {
                o.extend(e.clone());
            }
            self.ext_ask(&p.ext, method, params, waiter());
        }
        providers.len()
    }

    pub(super) fn ext_provide_hover(&mut self, doc: usize, pos: Pos) {
        self.ext_provide("hover", "provideHover", doc, pos, json!({}), || super::ext_host::Waiter::Hover(doc, pos));
    }

    pub(super) fn ext_provide_completion(&mut self, doc: usize, pos: Pos, trigger: Option<&str>, seq: u64) -> usize {
        self.ext_provide("completion", "provideCompletionItems", doc, pos, json!({ "triggerCharacter": trigger }), || super::ext_host::Waiter::Completion(seq))
    }

    /// Go to Definition: the providers, when there's no language server; else after it found
    /// nothing (`ext_definition_fallback`).
    pub(super) fn ext_provide_definition(&mut self, doc: usize, pos: Pos, has_server: bool) -> bool {
        if has_server {
            let asks = !self.ext_providers_for("definition", doc).is_empty();
            self.ext_languages.definition_fallback = asks.then_some((doc, pos));
            return false;
        }
        self.ext_provide("definition", "provideDefinition", doc, pos, json!({}), || super::ext_host::Waiter::Definition) > 0
    }

    pub(super) fn ext_has_code_actions(&self, doc: usize) -> bool {
        !self.ext_providers_for("codeAction", doc).is_empty()
    }

    /// Asks the code action providers for document `doc` about `range`, with the
    /// `diagnostics` there (in `encoding`, sent as UTF-8). Returns how many were asked.
    pub(super) fn ext_provide_code_actions(&mut self, doc: usize, (a, z): (Pos, Pos), diagnostics: &[lsp::Diagnostic], encoding: Encoding, auto: Option<u64>) -> usize {
        let providers = self.ext_providers_for("codeAction", doc);
        let Some(d) = self.docs.get(doc).and_then(Option::as_ref).filter(|_| !providers.is_empty()) else { return 0 };
        let document = super::ext_host::doc_json(d, true);
        let at = |p: Pos| json!({ "line": p.line, "character": Encoding::Utf8.to_lsp(&d.buffer.line(p.line), p.col) });
        let range = json!({ "start": at(a), "end": at(z) });
        let utf8 = |p: lsp::Position| {
            let line = d.buffer.line(p.line as usize);
            json!({ "line": p.line, "character": Encoding::Utf8.to_lsp(&line, encoding.from_lsp(&line, p.character)) })
        };
        let diagnostics: Vec<Value> = diagnostics
            .iter()
            .map(|g| {
                let mut v = g.raw.clone();
                if !v.is_object() {
                    v = json!({ "message": g.message });
                }
                v["range"] = json!({ "start": utf8(g.range.start), "end": utf8(g.range.end) });
                v
            })
            .collect();
        for p in &providers {
            let params = json!({ "provider": p.id, "document": document, "range": range, "context": { "diagnostics": diagnostics } });
            self.ext_ask(&p.ext, "provideCodeActions", params, super::ext_host::Waiter::CodeActions { ext: p.ext.clone(), auto });
        }
        providers.len()
    }

    pub(super) fn ext_code_actions_answer(&mut self, ext: &str, auto: Option<u64>, result: Result<Value, String>) {
        let source = super::refactor::ActionSource::Extension(ext.to_string());
        let actions = result.map(|v| lsp::parse_code_actions(&v)).unwrap_or_default().into_iter().map(|a| (a, source.clone())).collect();
        match auto {
            None => self.code_action_answer(actions),
            Some(seq) => self.lightbulb_actions(seq, actions),
        }
    }

    /// Asks the first formatting provider for document `doc` to format it (or `range`); its
    /// edits arrive like a server's (`formatted`). False if there's none.
    pub(super) fn ext_provide_formatting(&mut self, doc: usize, range: Option<(Pos, Pos)>, save: bool) -> bool {
        let Some(p) = self.ext_providers_for("formatting", doc).into_iter().next() else { return false };
        let Some(d) = self.docs.get(doc).and_then(Option::as_ref) else { return false };
        let Some(path) = d.buffer.path().map(Path::to_path_buf) else { return false };
        let at = |p: Pos| json!({ "line": p.line, "character": Encoding::Utf8.to_lsp(&d.buffer.line(p.line), p.col) });
        let range = range.map(|(a, z)| json!({ "start": at(a), "end": at(z) }));
        let cfg = crate::config::get();
        let params = json!({
            "provider": p.id,
            "document": super::ext_host::doc_json(d, true),
            "options": { "tabSize": cfg.tab_size, "insertSpaces": cfg.insert_spaces },
            "range": range,
        });
        let waiter = super::ext_host::Waiter::Formatting { path, version: d.buffer.version(), save };
        self.ext_ask(&p.ext, "provideDocumentFormattingEdits", params, waiter);
        true
    }

    /// The language server found no definition: ask the providers. Returns whether it did.
    pub(super) fn ext_definition_fallback(&mut self) -> bool {
        let Some((doc, pos)) = self.ext_languages.definition_fallback.take() else { return false };
        self.ext_provide("definition", "provideDefinition", doc, pos, json!({}), || super::ext_host::Waiter::Definition) > 0
    }

    pub(super) fn ext_hover_answer(&mut self, doc: usize, pos: Pos, result: Result<Value, String>) {
        let Some(markdown) = result.ok().and_then(|v| lsp::parse_hover(&v)).filter(|m| !m.trim().is_empty()) else { return };
        let Some(path) = self.docs.get(doc).and_then(Option::as_ref).and_then(|d| d.buffer.path().map(Path::to_path_buf)) else {
            return;
        };
        self.add_hover(&path, pos, markdown);
    }

    pub(super) fn ext_completion_answer(&mut self, seq: u64, result: Result<Value, String>) {
        let Ok(v) = result else { return };
        let (items, _) = lsp::parse_completions(&v);
        if !items.is_empty() {
            self.add_ext_completions(seq, items);
        }
    }

    pub(super) fn ext_definition_answer(&mut self, result: Result<Value, String>) {
        let locations = result.map(|v| lsp::parse_locations(&v)).unwrap_or_default();
        if locations.is_empty() {
            return self.set_status_message("No definition found");
        }
        self.handle_definition(locations, Encoding::Utf8);
    }

    /// `languages/setDiagnostics`.
    pub(super) fn ext_set_diagnostics(&mut self, ext: &str, params: &Value) {
        let (Some(collection), Some(path)) = (params["collection"].as_str(), params["path"].as_str()) else { return };
        let path = PathBuf::from(path);
        let diags = lsp::parse_diagnostics(&json!({ "uri": lsp::path_to_uri(&path), "diagnostics": params["diagnostics"] })).map(|(_, d)| d).unwrap_or_default();
        let files = self.ext_languages.diagnostics.entry((ext.to_string(), collection.to_string())).or_default();
        if diags.is_empty() {
            files.remove(&path);
        } else {
            files.insert(path.clone(), diags);
        }
        self.ext_merge_diagnostics(&path);
    }

    /// `languages/clearDiagnostics`.
    pub(super) fn ext_clear_diagnostics(&mut self, ext: &str, params: &Value) {
        let Some(collection) = params["collection"].as_str() else { return };
        let paths: Vec<PathBuf> = self.ext_languages.diagnostics.remove(&(ext.to_string(), collection.to_string())).map(|f| f.into_keys().collect()).unwrap_or_default();
        for path in paths {
            self.ext_merge_diagnostics(&path);
        }
    }

    /// Servers published diagnostics: put the extensions' back next to them.
    pub(super) fn ext_diagnostics_republished(&mut self) {
        let paths: Vec<PathBuf> = self.lsp.published.iter().filter(|p| self.ext_languages.diagnostics.values().any(|f| f.contains_key(*p))).cloned().collect();
        for path in paths {
            self.ext_merge_diagnostics(&path);
        }
    }

    /// Rebuilds `path`'s diagnostics: the server's, plus the extensions' converted to the
    /// server's position encoding.
    fn ext_merge_diagnostics(&mut self, path: &Path) {
        let (encoding, mut list) = self.lsp.diagnostics.remove(path).unwrap_or((Encoding::Utf8, Vec::new()));
        list.retain(|d| d.raw.get("orbvaneCollection").is_none());
        let ours: Vec<(String, lsp::Diagnostic)> = self
            .ext_languages
            .diagnostics
            .iter()
            .flat_map(|((_, collection), files)| files.get(path).into_iter().flatten().map(move |d| (collection.clone(), d.clone())))
            .collect();
        if !ours.is_empty() {
            let open = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path)).map(|d| d.buffer.text());
            let text = open.or_else(|| std::fs::read_to_string(path).ok()).unwrap_or_default();
            let lines: Vec<&str> = text.split('\n').collect();
            let convert = |p: lsp::Position| {
                let line = lines.get(p.line as usize).copied().unwrap_or("");
                let col = Encoding::Utf8.from_lsp(line, p.character);
                lsp::Position { line: p.line, character: encoding.to_lsp(line, col) }
            };
            for (collection, mut d) in ours {
                d.range.start = convert(d.range.start);
                d.range.end = convert(d.range.end);
                if let Value::Object(o) = &mut d.raw {
                    o.insert("orbvaneCollection".into(), json!(collection));
                }
                list.push(d);
            }
        }
        if !list.is_empty() {
            self.lsp.diagnostics.insert(path.to_path_buf(), (encoding, list));
        }
        // Positions are current: don't shift them by edits made before.
        self.diag_seq.remove(path);
    }
}
