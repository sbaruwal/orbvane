//! Semantic highlighting: colors from the language server's semantic tokens
//! (`textDocument/semanticTokens/full`), drawn over the tree-sitter colors,
//! `editor.semanticHighlighting`. Tokens are fetched for the documents on screen after typing
//! pauses, and follow edits until the next answer (edited lines lose theirs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp::Encoding;
use theme::Token;

use super::Workbench;

const DELAY: Duration = Duration::from_millis(300);
const RETRY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(10);

/// A token on a line: chars `start..end`, its kind (for the theme's colors), and its type and
/// modifiers (for the theme's `semanticTokenColors`).
#[derive(Clone, Debug, PartialEq)]
pub struct SemToken {
    pub start: usize,
    pub end: usize,
    pub token: Token,
    pub ty: u16,
    pub modifiers: u32,
}

/// A document's semantic tokens by line, the legend naming their types and modifiers, and a
/// generation that changes when they do.
#[derive(Clone, Default)]
pub struct Semantic {
    pub lines: Arc<HashMap<usize, Vec<SemToken>>>,
    pub legend: Arc<(Vec<String>, Vec<String>)>,
}

impl Semantic {
    pub fn on_line(&self, line: usize) -> &[SemToken] {
        self.lines.get(&line).map_or(&[], Vec::as_slice)
    }

    /// The token's type name and modifier names.
    pub fn names(&self, t: &SemToken) -> (&str, Vec<&str>) {
        let (types, mods) = &*self.legend;
        let ty = types.get(t.ty as usize).map_or("", String::as_str);
        let m = mods.iter().enumerate().filter(|(i, _)| t.modifiers & (1 << i) != 0).map(|(_, n)| n.as_str()).collect();
        (ty, m)
    }
}

/// Our token kind for a semantic token type, following the default scope mapping;
/// None leaves tree-sitter's color (comments, strings, numbers, operators...).
fn token_for(ty: &str, modifiers: &[&str]) -> Option<Token> {
    Some(match ty {
        "namespace" => Token::Namespace,
        "type" | "class" | "struct" | "enum" | "interface" | "typeAlias" | "builtinType" | "union" => Token::Type,
        "typeParameter" => Token::TypeParameter,
        "parameter" => Token::Parameter,
        "variable" if modifiers.iter().any(|m| matches!(*m, "readonly" | "constant" | "static")) => Token::Constant,
        "variable" => Token::Variable,
        "constParameter" => Token::Constant,
        "property" => Token::Property,
        "enumMember" => Token::EnumMember,
        "function" | "method" => Token::Function,
        "macro" => Token::Macro,
        "keyword" | "selfKeyword" | "selfTypeKeyword" => Token::Keyword,
        "lifetime" => Token::Lifetime,
        "attribute" | "decorator" | "derive" => Token::Attribute,
        _ => return None,
    })
}

/// Decodes the relative `data` (line delta, start delta, length, type, modifiers per token)
/// into tokens per line, with columns in chars of `line_text(line)`.
fn decode(data: &[u32], legend: &(Vec<String>, Vec<String>), encoding: Encoding, line_text: impl Fn(usize) -> String) -> HashMap<usize, Vec<SemToken>> {
    let mut out: HashMap<usize, Vec<SemToken>> = HashMap::new();
    let (mut line, mut start) = (0usize, 0u32);
    let mut cached: Option<(usize, String)> = None;
    for t in data.chunks_exact(5) {
        let (dl, ds, len, ty, mods) = (t[0], t[1], t[2], t[3], t[4]);
        if dl > 0 {
            line += dl as usize;
            start = 0;
        }
        start += ds;
        let names: Vec<&str> = legend.1.iter().enumerate().filter(|(i, _)| mods & (1 << i) != 0).map(|(_, n)| n.as_str()).collect();
        let Some(token) = legend.0.get(ty as usize).and_then(|n| token_for(n, &names)) else { continue };
        if cached.as_ref().is_none_or(|(l, _)| *l != line) {
            cached = Some((line, line_text(line)));
        }
        let text = &cached.as_ref().unwrap().1;
        let (a, z) = (encoding.from_lsp(text, start), encoding.from_lsp(text, start + len));
        if z > a {
            out.entry(line).or_default().push(SemToken { start: a, end: z, token, ty: ty as u16, modifiers: mods });
        }
    }
    out
}

#[derive(Default)]
pub(super) struct SemanticState {
    docs: HashMap<usize, DocTokens>,
    work_done: u64,
    enabled: bool,
}

impl SemanticState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
    }
}

#[derive(Default)]
struct DocTokens {
    semantic: Semantic,
    seq: Option<u64>,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    unsupported: bool,
    dirty: bool,
}

impl Workbench {
    fn semantic_enabled(&self) -> bool {
        match self.settings.get("editor.semanticHighlighting.enabled") {
            serde_json::Value::Bool(b) => b,
            serde_json::Value::String(s) if s == "true" => true,
            serde_json::Value::String(s) if s == "false" => false,
            _ => self.theme.semantic_highlighting,
        }
    }

    /// Follows edits and asks for tokens after a pause. Called every frame.
    pub(super) fn semantic_tick(&mut self) {
        let now = Instant::now();
        let enabled = self.semantic_enabled();
        if enabled != self.semantic.enabled {
            self.semantic.enabled = enabled;
            for st in self.semantic.docs.values_mut() {
                st.dirty = true;
                st.due = Some(now);
            }
        }
        if self.semantic.work_done != self.lsp.work_done {
            self.semantic.work_done = self.lsp.work_done;
            for st in self.semantic.docs.values_mut() {
                st.due = Some(now);
                st.unsupported = false;
            }
        }
        let on_screen: Vec<(usize, PathBuf)> = self.on_screen_docs();
        let gone: Vec<usize> = self.semantic.docs.keys().copied().filter(|d| !on_screen.iter().any(|(v, _)| v == d)).collect();
        for d in gone {
            self.semantic.docs.remove(&d);
            if let Some(doc) = self.docs.get_mut(d).and_then(Option::as_mut) {
                doc.semantic = Semantic::default();
            }
        }
        for (doc_id, path) in on_screen {
            let Some(doc) = self.docs[doc_id].as_ref() else { continue };
            let st = self.semantic.docs.entry(doc_id).or_default();
            let seq = doc.buffer.edit_seq();
            match st.seq {
                None => st.due = Some(now),
                Some(old) if old != seq => {
                    shift(&mut st.semantic, &doc.buffer, old);
                    st.dirty = true;
                    st.due = Some(now + DELAY);
                }
                _ => {}
            }
            st.seq = Some(seq);
            if st.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
                st.in_flight = None;
            }
            if !enabled || st.unsupported || st.in_flight.is_some() || !st.due.is_some_and(|t| now >= t) {
                continue;
            }
            st.due = None;
            if !self.lsp.is_running(&path) {
                if self.lsp.has_server(&path) {
                    self.semantic.docs.get_mut(&doc_id).unwrap().due = Some(now + RETRY);
                }
                continue;
            }
            let doc = self.docs[doc_id].as_ref().unwrap();
            let st = self.semantic.docs.get_mut(&doc_id).unwrap();
            if self.lsp.semantic_tokens(&path, &doc.buffer) {
                st.in_flight = Some(now);
            } else {
                st.unsupported = true;
            }
        }
        self.publish_semantic();
    }

    /// The documents shown in some group's active tab: (doc, path).
    pub(super) fn on_screen_docs(&self) -> Vec<(usize, PathBuf)> {
        let mut out: Vec<(usize, PathBuf)> = Vec::new();
        for gr in &self.groups {
            let Some(ed) = gr.tabs.get(gr.active).filter(|e| !e.is_special()) else { continue };
            let Some(path) = self.docs[ed.doc].as_ref().and_then(|d| d.buffer.path()) else { continue };
            if !out.iter().any(|(d, _)| *d == ed.doc) {
                out.push((ed.doc, path.to_path_buf()));
            }
        }
        out
    }

    fn publish_semantic(&mut self) {
        let enabled = self.semantic.enabled;
        for (&doc_id, st) in &mut self.semantic.docs {
            if !std::mem::take(&mut st.dirty) {
                continue;
            }
            if let Some(doc) = self.docs[doc_id].as_mut() {
                doc.semantic = if enabled { st.semantic.clone() } else { Semantic::default() };
            }
        }
    }

    pub(super) fn semantic_deadline(&self) -> Option<Instant> {
        self.semantic.docs.values().filter_map(|st| st.due).min()
    }

    pub(super) fn semantic_arrived(&mut self, path: &Path, version: u64, data: Option<Vec<u32>>, legend: Arc<(Vec<String>, Vec<String>)>, encoding: Encoding) {
        let Some(doc_id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        let Some(st) = self.semantic.docs.get_mut(&doc_id) else { return };
        let doc = self.docs[doc_id].as_ref().unwrap();
        st.in_flight = None;
        let Some(data) = data else {
            st.due = Some(Instant::now() + RETRY);
            return;
        };
        if version != doc.buffer.version() {
            st.due.get_or_insert(Instant::now());
            return;
        }
        let b = &doc.buffer;
        let lines = decode(&data, &legend, encoding, |l| if l < b.len_lines() { b.line(l).to_string() } else { String::new() });
        st.semantic = Semantic { lines: Arc::new(lines), legend };
        st.seq = Some(b.edit_seq());
        st.dirty = true;
        self.publish_semantic();
    }
}

/// Moves tokens with the edits since `seq`: lines after an edit shift, edited lines lose theirs.
fn shift(s: &mut Semantic, b: &text::Buffer, seq: u64) {
    let Some(edits) = b.edits_since(seq) else {
        s.lines = Arc::default();
        return;
    };
    let mut lines: HashMap<usize, Vec<SemToken>> = (*s.lines).clone();
    for change in edits {
        let text::Change::Edit(e) = change else {
            s.lines = Arc::default();
            return;
        };
        let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
        let delta = new_end as isize - old_end as isize;
        lines = lines
            .into_iter()
            .filter(|(l, _)| *l < start || *l > old_end)
            .map(|(l, t)| (if l > old_end { (l as isize + delta) as usize } else { l }, t))
            .collect();
    }
    s.lines = Arc::new(lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_relative_tokens() {
        let legend = (vec!["function".into(), "parameter".into(), "comment".into(), "variable".into()], vec!["declaration".into(), "readonly".into()]);
        // Line 0: `fn add(a` — "add" at 3 (function), "a" at 7 (parameter); line 2: a comment
        // (skipped), then a readonly variable at 4.
        let data = [0, 3, 3, 0, 1, 0, 4, 1, 1, 0, 2, 0, 2, 2, 0, 0, 4, 1, 3, 2];
        let lines = ["fn add(a", "", "// x yz", ""];
        let out = decode(&data, &legend, Encoding::Utf16, |l| lines[l].to_string());
        assert_eq!(out[&0].iter().map(|t| (t.start, t.end, t.token)).collect::<Vec<_>>(), [(3, 6, Token::Function), (7, 8, Token::Parameter)]);
        assert_eq!(out[&2][0].token, Token::Constant);
    }
}
