//! Extensions' text editor decorations: a type says how ranges look (background, text color, border, underline,
//! whole lines, a scroll bar mark, text before or after), and each file gets a set of ranges per
//! type. Ranges are kept as byte offsets that move with edits (through the buffer's edit log)
//! until the extension sets them again; files that aren't open keep the positions as sent.

use std::collections::HashMap;
use std::path::PathBuf;

use lsp::Encoding;
use serde_json::Value;
use text::{Change, Pos};

use super::Workbench;
use crate::editor::{Doc, ExtDeco};
use crate::layout::InlayHint;

/// A color as sent: a theme color id, or a fixed color.
#[derive(Clone, Debug)]
enum ColorSpec {
    Theme(String),
    Fixed(theme::Color),
}

impl ColorSpec {
    fn parse(v: &Value) -> Option<ColorSpec> {
        match v {
            Value::String(s) => theme::Color::hex(s).map(ColorSpec::Fixed),
            Value::Object(_) => v["id"].as_str().map(|id| ColorSpec::Theme(id.to_string())),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Attachment {
    text: String,
    color: Option<ColorSpec>,
}

#[derive(Clone, Debug)]
struct DecoType {
    background: Option<ColorSpec>,
    color: Option<ColorSpec>,
    border: Option<ColorSpec>,
    underline: bool,
    strike: bool,
    whole_line: bool,
    ruler: Option<ColorSpec>,
    before: Option<Attachment>,
    after: Option<Attachment>,
}

impl DecoType {
    fn parse(o: &Value) -> DecoType {
        let attachment = |v: &Value| v["contentText"].as_str().filter(|t| !t.is_empty()).map(|t| Attachment { text: t.to_string(), color: ColorSpec::parse(&v["color"]) });
        let text_decoration = o["textDecoration"].as_str().unwrap_or("");
        DecoType {
            background: ColorSpec::parse(&o["backgroundColor"]),
            color: ColorSpec::parse(&o["color"]),
            border: ColorSpec::parse(&o["borderColor"]),
            underline: text_decoration.contains("underline"),
            strike: text_decoration.contains("line-through"),
            whole_line: o["isWholeLine"].as_bool().unwrap_or(false),
            ruler: ColorSpec::parse(&o["overviewRulerColor"]),
            before: attachment(&o["before"]),
            after: attachment(&o["after"]),
        }
    }
}

/// One file's ranges of one decoration type.
struct DecoSet {
    ext: String,
    key: String,
    path: PathBuf,
    /// The ranges as sent: (start, end) as (line, UTF-8 column), and the hover message.
    raw: Vec<((u32, u32), (u32, u32), Option<String>)>,
    /// The ranges in the open document: (document, edit sequence they're current at, byte ranges).
    resolved: Option<(usize, u64, Vec<(usize, usize)>)>,
}

#[derive(Default)]
pub(super) struct ExtDecorations {
    types: HashMap<(String, String), DecoType>,
    sets: Vec<DecoSet>,
    /// Bumped when anything changes, so documents' before/after texts are rebuilt.
    generation: u64,
    /// The generation and buffer version each document's texts were built at.
    built: HashMap<usize, (u64, u64)>,
}

impl ExtDecorations {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.built.clear();
    }
}

fn point(v: &Value) -> (u32, u32) {
    (v["line"].as_u64().unwrap_or(0) as u32, v["character"].as_u64().unwrap_or(0) as u32)
}

/// Where a byte offset goes after an edit replacing `start..old_end` with `start..new_end`.
fn moved(p: usize, start: usize, old_end: usize, new_end: usize, is_end: bool) -> usize {
    if p <= start {
        p
    } else if p >= old_end {
        p + new_end - old_end
    } else if is_end {
        new_end
    } else {
        start
    }
}

impl Workbench {
    /// `window/createTextEditorDecorationType`.
    pub(super) fn ext_create_decoration_type(&mut self, ext: &str, params: &Value) {
        let Some(key) = params["key"].as_str() else { return };
        self.ext_decorations.types.insert((ext.to_string(), key.to_string()), DecoType::parse(&params["options"]));
        self.ext_decorations.generation += 1;
    }

    /// `window/disposeDecorationType`.
    pub(super) fn ext_dispose_decoration_type(&mut self, ext: &str, params: &Value) {
        let Some(key) = params["key"].as_str() else { return };
        let d = &mut self.ext_decorations;
        d.types.remove(&(ext.to_string(), key.to_string()));
        d.sets.retain(|s| !(s.ext == ext && s.key == key));
        d.generation += 1;
    }

    /// `window/setDecorations`: replaces a file's ranges of one type.
    pub(super) fn ext_set_decorations(&mut self, ext: &str, params: &Value) {
        let (Some(key), Some(path)) = (params["key"].as_str(), params["path"].as_str()) else { return };
        let path = PathBuf::from(path);
        let raw: Vec<_> = params["decorations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                let r = if d["range"].is_object() { &d["range"] } else { d };
                (point(&r["start"]), point(&r["end"]), d["hoverMessage"].as_str().map(String::from))
            })
            .collect();
        let d = &mut self.ext_decorations;
        d.sets.retain(|s| !(s.ext == ext && s.key == key && s.path == path));
        if !raw.is_empty() {
            d.sets.push(DecoSet { ext: ext.to_string(), key: key.to_string(), path, raw, resolved: None });
        }
        d.generation += 1;
    }

    /// An extension stopped: its decorations go.
    pub(super) fn ext_forget_decorations(&mut self, ext: &str) {
        let d = &mut self.ext_decorations;
        d.types.retain(|(e, _), _| e != ext);
        d.sets.retain(|s| s.ext != ext);
        d.generation += 1;
    }

    /// Each frame: moves ranges with edits, and rebuilds documents' before/after texts.
    pub(super) fn ext_decorations_tick(&mut self) {
        if self.ext_decorations.sets.is_empty() && self.ext_decorations.built.is_empty() {
            return;
        }
        let mut changed = false;
        for i in 0..self.ext_decorations.sets.len() {
            let path = self.ext_decorations.sets[i].path.clone();
            let doc_id = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path.as_path())));
            let set = &mut self.ext_decorations.sets[i];
            let Some(id) = doc_id else {
                if set.resolved.take().is_some() {
                    changed = true;
                }
                continue;
            };
            let buffer = &self.docs[id].as_ref().unwrap().buffer;
            let seq = buffer.edit_seq();
            if let Some((doc, at, ranges)) = &mut set.resolved {
                if *doc == id && *at == seq {
                    continue;
                }
                let edits = (*doc == id).then(|| buffer.edits_since(*at)).flatten();
                if let Some(edits) = edits.filter(|e| !e.iter().any(|c| matches!(c, Change::Reset))) {
                    for change in edits {
                        if let Change::Edit(e) = change {
                            for r in ranges.iter_mut() {
                                r.0 = moved(r.0, e.start_byte, e.old_end_byte, e.new_end_byte, false);
                                r.1 = moved(r.1, e.start_byte, e.old_end_byte, e.new_end_byte, true).max(r.0);
                            }
                        }
                    }
                    *at = seq;
                    changed = true;
                    continue;
                }
            }
            // First time here (or after undo/redo): from the positions as sent.
            let byte = |(line, ch): (u32, u32)| {
                let line = (line as usize).min(buffer.len_lines().saturating_sub(1));
                let col = Encoding::Utf8.from_lsp(&buffer.line(line), ch);
                buffer.byte_of(Pos::new(line, col))
            };
            let ranges = set.raw.iter().map(|(a, z, _)| (byte(*a), byte(*z).max(byte(*a)))).collect();
            set.resolved = Some((id, seq, ranges));
            changed = true;
        }
        if changed {
            self.ext_decorations.generation += 1;
        }
        self.ext_decoration_texts();
    }

    /// Rebuilds the before/after texts of documents whose decorations or text changed.
    fn ext_decoration_texts(&mut self) {
        let generation = self.ext_decorations.generation;
        for id in 0..self.docs.len() {
            let Some(doc) = self.docs[id].as_ref() else {
                self.ext_decorations.built.remove(&id);
                continue;
            };
            let key = (generation, doc.buffer.version());
            if self.ext_decorations.built.get(&id) == Some(&key) {
                continue;
            }
            let has = self.ext_decorations.sets.iter().any(|s| s.resolved.as_ref().is_some_and(|r| r.0 == id));
            if !has && !self.ext_decorations.built.contains_key(&id) {
                continue;
            }
            let mut lines: HashMap<usize, Vec<InlayHint>> = HashMap::new();
            for set in &self.ext_decorations.sets {
                let (Some((doc_id, _, ranges)), Some(ty)) = (&set.resolved, self.ext_decorations.types.get(&(set.ext.clone(), set.key.clone()))) else { continue };
                if *doc_id != id || (ty.before.is_none() && ty.after.is_none()) {
                    continue;
                }
                for &(a, z) in ranges {
                    for (at, att) in [(a, &ty.before), (z, &ty.after)] {
                        let Some(att) = att else { continue };
                        let pos = doc.buffer.pos_of_byte(at.min(doc.buffer.len_bytes()));
                        let color = Some(att.color.as_ref().map_or_else(|| self.theme.color("editorCodeLens.foreground"), |c| self.resolve_color(c)));
                        lines.entry(pos.line).or_default().push(InlayHint { col: pos.col, label: att.text.clone(), parameter: false, pad_left: false, pad_right: false, swatch: None, color });
                    }
                }
            }
            for v in lines.values_mut() {
                v.sort_by_key(|h| h.col);
            }
            let doc = self.docs[id].as_mut().unwrap();
            if !(lines.is_empty() && doc.ext_inlays.lines.is_empty()) {
                doc.ext_inlays = crate::layout::Inlays { lines: std::sync::Arc::new(lines), generation: doc.ext_inlays.generation + 1 };
            }
            if has {
                self.ext_decorations.built.insert(id, key);
            } else {
                self.ext_decorations.built.remove(&id);
            }
        }
    }

    fn resolve_color(&self, c: &ColorSpec) -> theme::Color {
        match c {
            ColorSpec::Theme(id) => self.theme.color(id),
            ColorSpec::Fixed(c) => *c,
        }
    }

    /// The decorations to draw in document `id`.
    pub(super) fn ext_decorations_for(&self, id: usize) -> Vec<ExtDeco> {
        let Some(doc) = self.docs.get(id).and_then(Option::as_ref) else { return Vec::new() };
        let mut out = Vec::new();
        for set in &self.ext_decorations.sets {
            let (Some((doc_id, _, ranges)), Some(ty)) = (&set.resolved, self.ext_decorations.types.get(&(set.ext.clone(), set.key.clone()))) else { continue };
            if *doc_id != id {
                continue;
            }
            let color = |c: &Option<ColorSpec>| c.as_ref().map(|c| self.resolve_color(c));
            for &(a, z) in ranges {
                out.push(ExtDeco {
                    start: pos(doc, a),
                    end: pos(doc, z),
                    background: color(&ty.background),
                    color: color(&ty.color),
                    border: color(&ty.border),
                    underline: ty.underline,
                    strike: ty.strike,
                    whole_line: ty.whole_line,
                    ruler: color(&ty.ruler),
                });
            }
        }
        out
    }

    /// The hover messages of decorations at `at` in document `id`.
    pub(super) fn ext_decoration_hovers(&self, id: usize, at: Pos) -> Vec<String> {
        let Some(doc) = self.docs.get(id).and_then(Option::as_ref) else { return Vec::new() };
        let byte = doc.buffer.byte_of(at);
        let mut out = Vec::new();
        for set in &self.ext_decorations.sets {
            let Some((doc_id, _, ranges)) = &set.resolved else { continue };
            if *doc_id != id {
                continue;
            }
            for (k, &(a, z)) in ranges.iter().enumerate() {
                if a <= byte && byte <= z {
                    if let Some(Some(msg)) = set.raw.get(k).map(|r| r.2.clone()) {
                        out.push(msg);
                    }
                }
            }
        }
        out
    }
}

fn pos(doc: &Doc, byte: usize) -> Pos {
    doc.buffer.pos_of_byte(byte.min(doc.buffer.len_bytes()))
}

#[cfg(test)]
mod tests {
    use super::moved;

    #[test]
    fn ranges_follow_edits() {
        // Typing before a range moves it; typing inside grows it; deleting over its start
        // clamps it.
        assert_eq!(moved(10, 2, 2, 5, false), 13);
        assert_eq!((moved(10, 12, 12, 15, false), moved(20, 12, 12, 15, true)), (10, 23));
        assert_eq!((moved(10, 8, 12, 8, false), moved(20, 8, 12, 8, true)), (8, 16));
        // Typing right after the end doesn't extend it.
        assert_eq!(moved(20, 20, 20, 23, true), 20);
    }
}
