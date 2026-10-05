//! Inline color swatches: a small
//! square before each color value in the documents on screen. Colors come from the language
//! server (`textDocument/documentColor`) and, per `editor.defaultColorDecorators`, from
//! `crate::colors`, which finds hex, `rgb()` and `hsl()` values in any text. Swatches are
//! handed to the editor through `Doc::swatches` and take columns like inlay hints.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp::Encoding;

use super::inlays::shift;
use super::Workbench;
use crate::config::{self, DefaultColors};
use crate::layout::{InlayHint, Inlays};

/// How long typing pauses before a server is asked again.
const DELAY: Duration = Duration::from_millis(300);
/// When a server couldn't answer (still loading), ask again after this long.
const RETRY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(10);

type Lines = HashMap<usize, Vec<InlayHint>>;

#[derive(Default)]
pub(super) struct ColorDecorators {
    docs: HashMap<usize, DocColors>,
    generation: u64,
    /// The settings the swatches were made with: (enabled, default colors, limit).
    settings: Option<(bool, DefaultColors, usize)>,
    work_done: u64,
}

impl ColorDecorators {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(super) fn forget_docs(&mut self) {
        self.docs.clear();
    }
}

#[derive(Default)]
struct DocColors {
    /// The buffer version the text's own colors were found in.
    version: Option<u64>,
    /// The edit sequence the server's colors have been moved to.
    seq: Option<u64>,
    /// Colors from the server and from the text.
    server: Lines,
    text: Lines,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    /// The server offers colors (known once it runs).
    has_server: bool,
    dirty: bool,
}

/// Swatches for the colors `crate::colors` finds in `doc`, at most `limit`.
fn text_colors(buffer: &text::Buffer, limit: usize) -> Lines {
    let mut out = Lines::new();
    let mut n = 0;
    for line in 0..buffer.len_lines() {
        let text = buffer.line_cow(line);
        if !text.contains('#') && !text.contains('(') {
            continue;
        }
        for f in crate::colors::find_in_line(&text) {
            if n == limit {
                return out;
            }
            n += 1;
            out.entry(line).or_default().push(InlayHint::swatch(f.start, f.end, f.color));
        }
    }
    out
}

impl Workbench {
    /// The documents in the editors on screen (untitled ones too, for the text's colors).
    fn documents_on_screen(&self) -> Vec<(usize, Option<PathBuf>)> {
        let mut out: Vec<(usize, Option<PathBuf>)> = Vec::new();
        for gr in &self.groups {
            let Some(ed) = gr.tabs.get(gr.active).filter(|e| !e.is_special() && e.search.is_none()) else { continue };
            let Some(doc) = self.docs[ed.doc].as_ref().filter(|d| !d.large) else { continue };
            if !out.iter().any(|(d, _)| *d == ed.doc) {
                out.push((ed.doc, doc.buffer.path().map(PathBuf::from)));
            }
        }
        out
    }

    /// Keeps the swatches of the documents on screen up to date. Called every frame.
    pub(super) fn color_tick(&mut self) {
        let now = Instant::now();
        let cfg = config::get();
        let settings = (cfg.color_decorators, cfg.default_colors, cfg.color_limit);
        if self.colors.settings != Some(settings) {
            self.colors.settings = Some(settings);
            for d in std::mem::take(&mut self.colors.docs).into_keys() {
                self.set_swatches(d, Lines::new());
            }
        }
        if self.colors.work_done != self.lsp.work_done {
            // The server may know more now (answers are empty while it loads).
            self.colors.work_done = self.lsp.work_done;
            for st in self.colors.docs.values_mut().filter(|st| st.has_server) {
                st.due = Some(now);
            }
        }
        let on_screen = if cfg.color_decorators { self.documents_on_screen() } else { Vec::new() };
        let gone: Vec<usize> = self.colors.docs.keys().copied().filter(|d| !on_screen.iter().any(|(v, _)| v == d)).collect();
        for d in gone {
            self.colors.docs.remove(&d);
            self.set_swatches(d, Lines::new());
        }
        for (id, path) in on_screen {
            let Some(doc) = self.docs[id].as_ref() else { continue };
            let has_server = path.as_deref().is_some_and(|p| self.lsp.supports(p, "colorProvider"));
            let st = self.colors.docs.entry(id).or_default();
            if has_server && !st.has_server {
                st.due = Some(now);
            }
            st.has_server = has_server;
            // The text's colors: found again whenever it changes (cheap enough per keystroke).
            let text_wanted = match cfg.default_colors {
                DefaultColors::Always => true,
                DefaultColors::Auto => !has_server,
                DefaultColors::Never => false,
            };
            let version = doc.buffer.version();
            if !text_wanted && !st.text.is_empty() {
                st.text.clear();
                st.dirty = true;
            }
            if text_wanted && st.version != Some(version) {
                st.text = text_colors(&doc.buffer, cfg.color_limit);
                st.dirty = true;
            }
            st.version = Some(version);
            // The server's colors move with edits until it answers again.
            let seq = doc.buffer.edit_seq();
            if let Some(old) = st.seq.filter(|&old| old != seq) {
                shift(&mut st.server, &doc.buffer, old);
                st.dirty = true;
                if has_server {
                    st.due = Some(now + DELAY);
                }
            }
            st.seq = Some(seq);
            if !has_server && !st.server.is_empty() {
                st.server.clear();
                st.dirty = true;
            }
            if st.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
                st.in_flight = None;
            }
            if has_server && st.in_flight.is_none() && st.due.is_some_and(|t| now >= t) {
                st.due = None;
                let path = path.unwrap();
                if !self.lsp.is_running(&path) {
                    self.colors.docs.get_mut(&id).unwrap().due = Some(now + RETRY);
                } else if self.lsp.document_colors(&path, &self.docs[id].as_ref().unwrap().buffer) {
                    self.colors.docs.get_mut(&id).unwrap().in_flight = Some(now);
                }
            }
        }
        self.publish_swatches();
    }

    /// The server's colors for `path` at buffer version `version`.
    pub(super) fn colors_arrived(&mut self, path: PathBuf, version: u64, colors: Option<Vec<(lsp::Range, [f32; 4])>>, encoding: Encoding) {
        let limit = config::get().color_limit;
        let Some(id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path.as_path()))) else { return };
        let Some(st) = self.colors.docs.get_mut(&id) else { return };
        st.in_flight = None;
        let b = &self.docs[id].as_ref().unwrap().buffer;
        let Some(colors) = colors.filter(|_| b.version() == version) else {
            // Failed (loading) or answered for older text: ask again.
            st.due = Some(Instant::now() + if b.version() == version { RETRY } else { Duration::ZERO });
            return;
        };
        let mut lines = Lines::new();
        for (range, [r, g, b_, a]) in colors.into_iter().take(limit) {
            let line = range.start.line as usize;
            if line >= b.len_lines() {
                continue;
            }
            let text = b.line(line);
            let col = encoding.from_lsp(&text, range.start.character);
            let end = if range.end.line as usize == line { encoding.from_lsp(&text, range.end.character) } else { text.chars().count() };
            lines.entry(line).or_default().push(InlayHint::swatch(col, end, theme::Color { r, g, b: b_, a }));
        }
        for v in lines.values_mut() {
            v.sort_by_key(|h| h.col);
        }
        st.server = lines;
        st.seq = Some(b.edit_seq());
        st.dirty = true;
        self.publish_swatches();
    }

    /// Hands changed swatches to their documents.
    fn publish_swatches(&mut self) {
        let dirty: Vec<usize> = self.colors.docs.iter().filter(|(_, st)| st.dirty).map(|(&d, _)| d).collect();
        for d in dirty {
            let st = self.colors.docs.get_mut(&d).unwrap();
            st.dirty = false;
            let mut all = st.server.clone();
            for (line, hints) in &st.text {
                let v = all.entry(*line).or_default();
                // The server's color wins where both found one.
                v.extend(hints.iter().filter(|h| !st.server.get(line).is_some_and(|s| s.iter().any(|x| x.col == h.col))).cloned());
                v.sort_by_key(|h| h.col);
            }
            self.set_swatches(d, all);
        }
    }

    fn set_swatches(&mut self, doc: usize, lines: Lines) {
        let Some(d) = self.docs.get_mut(doc).and_then(|d| d.as_mut()) else { return };
        if lines.is_empty() && d.swatches.lines.is_empty() {
            return;
        }
        self.colors.generation += 1;
        d.swatches = Inlays { lines: Arc::new(lines), generation: self.colors.generation };
    }

    pub(super) fn color_deadline(&self) -> Option<Instant> {
        self.colors.docs.values().filter(|st| st.in_flight.is_none()).filter_map(|st| st.due).min()
    }
}
