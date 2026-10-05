//! Snippets: completion items from language servers with tab stops, and the user's snippet
//! files, offered in
//! completion and by Insert Snippet. While a snippet is active, Tab and ⇧Tab move between its
//! placeholders (a placeholder used twice is edited in both places), Escape leaves it.

use std::ops::Range;
use std::path::{Path, PathBuf};

use text::{Change, Pos, Selection};

use super::Workbench;
use crate::snippet::{self, Variables};

/// An active snippet.
pub(super) struct SnippetSession {
    group: usize,
    doc: usize,
    /// Tab stops as byte ranges in the document, in Tab order (the last is the final cursor).
    stops: Vec<Vec<Range<usize>>>,
    current: usize,
    /// The document's edit sequence the ranges are up to date with.
    seq: u64,
}

/// A user snippet.
#[derive(Clone, Debug)]
pub(super) struct UserSnippet {
    pub name: String,
    pub prefix: String,
    pub body: String,
    pub description: String,
}

/// The variables of the editor a snippet goes into.
struct EditorVars {
    path: Option<PathBuf>,
    line: String,
    line_index: usize,
    word: String,
    selected: String,
}

impl Variables for EditorVars {
    fn get(&self, name: &str) -> Option<String> {
        let file = || self.path.as_deref();
        Some(match name {
            "TM_FILENAME" => file()?.file_name()?.to_string_lossy().into_owned(),
            "TM_FILENAME_BASE" => file()?.file_stem()?.to_string_lossy().into_owned(),
            "TM_DIRECTORY" => file()?.parent()?.display().to_string(),
            "TM_FILEPATH" => file()?.display().to_string(),
            "TM_LINE_INDEX" => self.line_index.to_string(),
            "TM_LINE_NUMBER" => (self.line_index + 1).to_string(),
            "TM_CURRENT_LINE" => self.line.clone(),
            "TM_CURRENT_WORD" => self.word.clone(),
            "TM_SELECTED_TEXT" | "SELECTION" => self.selected.clone(),
            _ => return None,
        })
    }
}

/// Re-indents a snippet for where it goes: lines after the first get the current line's
/// indentation, and tabs become the editor's indent unit. Stop offsets move
/// with the text.
fn adjust_indentation(s: snippet::Snippet, line_indent: &str) -> snippet::Snippet {
    let unit = crate::editor::indent_unit();
    let mut text = String::with_capacity(s.text.len());
    let mut map = Vec::with_capacity(s.text.len() + 1); // old byte -> new byte
    for (i, c) in s.text.char_indices() {
        map.extend(std::iter::repeat_n(text.len(), c.len_utf8()));
        let _ = i;
        match c {
            '\n' => {
                text.push('\n');
                text.push_str(line_indent);
            }
            '\t' => text.push_str(&unit),
            _ => text.push(c),
        }
    }
    map.push(text.len());
    let stops = s.stops.into_iter().map(|rs| rs.into_iter().map(|r| map[r.start]..map[r.end]).collect()).collect();
    snippet::Snippet { text, stops }
}

/// Reads the snippets for language `lang_id`: the user's, then the
/// ones extensions contribute.
pub(super) fn user_snippets(lang_id: &str) -> Vec<UserSnippet> {
    let dir = settings::user_data_dir().join("User").join("snippets");
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // <language>.json, or a global *.code-snippets file (filtered by each snippet's scope).
        let global = name.ends_with(".code-snippets");
        if global || name == format!("{lang_id}.json") {
            files.push((path, global));
        }
    }
    files.extend(crate::contributions::snippet_files(lang_id));
    let mut out = Vec::new();
    for (path, global) in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&theme::strip_jsonc(&text)) else { continue };
        for (name, s) in map {
            if global {
                if let Some(scope) = s["scope"].as_str().filter(|sc| !sc.trim().is_empty()) {
                    if !scope.split(',').any(|l| l.trim() == lang_id) {
                        continue;
                    }
                }
            }
            let lines = |v: &serde_json::Value| match v {
                serde_json::Value::String(t) => Some(t.clone()),
                serde_json::Value::Array(a) => Some(a.iter().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n")),
                _ => None,
            };
            let Some(body) = lines(&s["body"]) else { continue };
            let prefixes: Vec<String> = match &s["prefix"] {
                serde_json::Value::String(p) => vec![p.clone()],
                serde_json::Value::Array(a) => a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            let description = s["description"].as_str().unwrap_or(&name).to_string();
            for prefix in prefixes {
                out.push(UserSnippet { name: name.clone(), prefix, body: body.clone(), description: description.clone() });
            }
        }
    }
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

impl Workbench {
    /// Replaces `start..end` in the active editor with snippet `src` and starts a session at its
    /// first placeholder (or just places the cursor if it has none).
    pub(super) fn insert_snippet(&mut self, start: Pos, end: Pos, src: &str) {
        let g = self.active_group;
        let Some((ed, doc)) = self.active_mut() else { return };
        let b = &doc.buffer;
        let line = b.line(start.line);
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let (a, z) = ed.sel.ordered();
        let vars = EditorVars {
            path: b.path().map(Path::to_path_buf),
            line: line.clone(),
            line_index: start.line,
            word: b.text_in(&b.word_at(start)),
            selected: if a != z { b.text_in(&Selection { anchor: a, head: z, goal_col: None }) } else { String::new() },
        };
        let s = adjust_indentation(snippet::parse(src, &vars), &indent);
        let base = doc.buffer.byte_of(start);
        doc.buffer.insert(Selection { anchor: start, head: end, goal_col: None }, &s.text);
        doc.buffer.break_undo_group();
        let stops: Vec<Vec<Range<usize>>> = s.stops.into_iter().map(|rs| rs.into_iter().map(|r| base + r.start..base + r.end).collect()).collect();
        let seq = doc.buffer.edit_seq();
        let doc_id = ed.doc;
        self.snippet = Some(SnippetSession { group: g, doc: doc_id, stops, current: 0, seq });
        self.select_stop(0);
    }

    /// Selects every range of stop `i`; the last stop ends the session.
    fn select_stop(&mut self, i: usize) {
        let Some(s) = &mut self.snippet else { return };
        s.current = i;
        let ranges = s.stops[i].clone();
        let last = i + 1 >= s.stops.len();
        let (g, doc_id) = (s.group, s.doc);
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active).filter(|e| e.doc == doc_id) else { return self.snippet = None };
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let b = &doc.buffer;
        let sels: Vec<Selection> = ranges.iter().map(|r| Selection { anchor: b.pos_of_byte(r.start), head: b.pos_of_byte(r.end), goal_col: None }).collect();
        ed.set_selections(sels);
        ed.reveal = true;
        if last {
            self.snippet = None;
        }
    }

    /// Tab / ⇧Tab in a snippet. Returns false when no snippet is active.
    pub(super) fn snippet_tab(&mut self, back: bool) -> bool {
        self.sync_snippet();
        let Some(s) = &self.snippet else { return false };
        let next = if back { s.current.saturating_sub(1) } else { s.current + 1 };
        self.select_stop(next.min(s.stops.len() - 1));
        true
    }

    pub(super) fn snippet_active(&self) -> bool {
        self.snippet.is_some()
    }

    pub(super) fn leave_snippet(&mut self) {
        self.snippet = None;
    }

    /// Moves the stops with edits to the document; an edit outside the current placeholder
    /// (or undo) ends the snippet.
    pub(super) fn sync_snippet(&mut self) {
        let Some(s) = &mut self.snippet else { return };
        let Some(doc) = self.docs.get(s.doc).and_then(Option::as_ref) else { return self.snippet = None };
        let seq = doc.buffer.edit_seq();
        if seq == s.seq {
            return;
        }
        let Some(edits) = doc.buffer.edits_since(s.seq) else { return self.snippet = None };
        s.seq = seq;
        for change in edits {
            let Change::Edit(e) = change else { return self.snippet = None };
            let (from, old_end) = (e.start_byte, e.old_end_byte);
            let delta = e.new_end_byte as isize - old_end as isize;
            // Only edits inside the current placeholder's ranges keep the snippet going.
            if !s.stops[s.current].iter().any(|r| r.start <= from && old_end <= r.end) {
                return self.snippet = None;
            }
            let shift = |p: usize| (p as isize + delta) as usize;
            for rs in &mut s.stops {
                for r in rs.iter_mut() {
                    if r.start <= from && old_end <= r.end {
                        r.end = shift(r.end); // typing inside it
                    } else if r.start >= old_end {
                        *r = shift(r.start)..shift(r.end);
                    }
                }
            }
        }
        // The cursor left the placeholder: done.
        let s = self.snippet.as_ref().unwrap();
        let gr = &self.groups[s.group];
        let inside = gr.tabs.get(gr.active).filter(|e| e.doc == s.doc).is_some_and(|ed| {
            let head = doc.buffer.byte_of(ed.sel.head);
            s.stops[s.current].iter().any(|r| r.start <= head && head <= r.end)
        });
        if !inside {
            self.snippet = None;
        }
    }

    /// Placeholders to highlight in group `g`'s editor: (range, is the final stop).
    pub(super) fn snippet_highlights(&self, g: usize, doc: usize) -> Vec<(Pos, Pos, bool)> {
        let Some(s) = self.snippet.as_ref().filter(|s| s.group == g && s.doc == doc) else { return Vec::new() };
        let Some(d) = self.docs[doc].as_ref() else { return Vec::new() };
        let last = s.stops.len() - 1;
        s.stops
            .iter()
            .enumerate()
            .flat_map(|(i, rs)| rs.iter().map(move |r| (r.clone(), i == last)))
            .map(|(r, fin)| (d.buffer.pos_of_byte(r.start), d.buffer.pos_of_byte(r.end), fin))
            .collect()
    }

    /// The user's snippets for the active editor's language, as completion items.
    pub(super) fn snippet_completions(&self) -> Vec<lsp::CompletionItem> {
        let Some(doc) = self.active_doc() else { return Vec::new() };
        user_snippets(doc.lang.id())
            .into_iter()
            .map(|s| lsp::CompletionItem {
                label: s.prefix.clone(),
                label_detail: None,
                detail: Some(s.description),
                kind: 15, // Snippet
                filter_text: s.prefix.clone(),
                sort_text: format!("~{}", s.prefix),
                documentation: Some(format!("```\n{}\n```", crate::snippet::parse(&s.body, &()).text)),
                insert_text: s.body,
                edit: None,
                additional_edits: Vec::new(),
                snippet: true,
                command: None,
            })
            .collect()
    }

    /// Insert Snippet: the user's snippets for this language in the quick input.
    pub(super) fn open_insert_snippet(&mut self) {
        let Some(doc) = self.active_doc() else { return };
        let choices = user_snippets(doc.lang.id())
            .into_iter()
            .map(|s| crate::palette::Item {
                label: s.prefix.clone(),
                detail: s.name.clone(),
                matches: Vec::new(),
                shortcut: None,
                action: crate::palette::Action::InsertSnippet(s.body),
                group: None,
                kind: Some(15),
            })
            .collect();
        let mut p = crate::palette::Palette::with_picker(crate::palette::Picker { placeholder: "Select a snippet".into(), choices });
        if p.items.is_empty() {
            p.message = Some("No snippet available".into());
        }
        self.palette = Some(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_following_lines() {
        let s = snippet::parse("if ${1:x} {\n\t$0\n}", &());
        let s = adjust_indentation(s, "    ");
        let unit = crate::editor::indent_unit();
        assert_eq!(s.text, format!("if x {{\n    {unit}\n    }}"));
        assert_eq!(s.stops[0], vec![3..4]);
        let end = 3 + 1 + 3 + 4 + unit.len();
        assert_eq!(s.stops[1], vec![end..end]);
    }
}
