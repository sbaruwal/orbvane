//! Side-by-side diff: an editor tab comparing an older version (left) with a newer one
//! (right), like the standard diff editor. Read-only; the right side can be the live document.

use std::path::{Path, PathBuf};

use language::{Highlighter, Lang, Span};
use render::{Canvas, Color, Rect, TextStyle};
use scm::DiffRow;
use text::{Buffer, Selection};
use theme::Theme;

use crate::editor::{expand_tabs_spans, Doc, font_size, line_height};

const SCROLLBAR_W: f32 = 14.0;

/// Which comparison a diff tab shows, so it can be reloaded when git state changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSpec {
    pub path: PathBuf,
    /// Staged: HEAD vs index. Otherwise: index vs working tree.
    pub staged: bool,
    /// A commit: what it changed in the file (its parent vs it). Such diffs never change.
    pub revision: Option<String>,
    /// Not git: this file (as saved on disk) on the left, the open `path` on the right
    /// (Compare with Selected; the file itself for Compare with Saved).
    pub left_file: Option<PathBuf>,
}

/// One side's text with its own highlighter (for text that isn't an open document).
struct Side {
    buffer: Buffer,
    highlight: Highlighter,
}

impl Side {
    fn new(text: &str, lang: Lang) -> Self {
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), text);
        let mut highlight = Highlighter::new(lang);
        highlight.update(&mut buffer);
        Self { buffer, highlight }
    }
}

pub struct DiffState {
    pub spec: DiffSpec,
    pub label: String,
    left: Side,
    /// Fixed right text; None means the right side is the live document.
    right: Option<Side>,
    rows: Vec<DiffRow>,
    /// Document version the rows were computed for (live right side).
    rows_version: Option<u64>,
    pub scroll_y: f32,
    pub scroll_x: f32,
    /// Where the diff was last drawn (for scroll hit-testing).
    pub view: Rect,
}

impl DiffState {
    /// Loads the texts for `spec` from git. For unstaged changes of a file that still exists,
    /// the right side is the live open document.
    pub fn load(root: &Path, spec: DiffSpec, lang: Lang) -> Self {
        let name = spec.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if let Some(left) = &spec.left_file {
            let text = std::fs::read_to_string(left).unwrap_or_default().replace("\r\n", "\n");
            let label = if *left == spec.path {
                format!("{name} (on disk) ↔ {name}")
            } else {
                format!("{} ↔ {name}", left.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()))
            };
            return Self::with_texts(spec.clone(), label, &text, None, lang);
        }
        let (left, right, suffix) = if let Some(rev) = &spec.revision {
            let short = &rev[..rev.len().min(7)];
            let before = scm::show(root, &format!("{rev}^"), &spec.path).unwrap_or_default();
            let after = scm::show(root, rev, &spec.path).unwrap_or_default();
            return Self::with_texts(spec.clone(), format!("{name} ({short}^ ↔ {short})"), &before, Some(&after), lang);
        } else if spec.staged {
            let head = scm::show(root, "HEAD", &spec.path).unwrap_or_default();
            let index = scm::show(root, "", &spec.path).unwrap_or_default();
            (head, Some(index), "Index")
        } else {
            // Unstaged: compare with the index (falls back to HEAD, then empty).
            let base = scm::show(root, "", &spec.path).or_else(|| scm::show(root, "HEAD", &spec.path)).unwrap_or_default();
            let exists = spec.path.is_file();
            (base, if exists { None } else { Some(String::new()) }, if exists { "Working Tree" } else { "Deleted" })
        };
        Self::with_texts(spec, format!("{name} ({suffix})"), &left, right.as_deref(), lang)
    }

    /// A diff of two fixed texts (`spec.revision` should be set so it's never reloaded).
    pub fn fixed(spec: DiffSpec, label: String, left: &str, right: &str, lang: Lang) -> Self {
        Self::with_texts(spec, label, left, Some(right), lang)
    }

    fn with_texts(spec: DiffSpec, label: String, left: &str, right: Option<&str>, lang: Lang) -> Self {
        Self {
            label,
            left: Side::new(left, lang),
            right: right.map(|t| Side::new(t, lang)),
            rows: Vec::new(),
            rows_version: None,
            scroll_y: 0.0,
            scroll_x: 0.0,
            view: Rect::default(),
            spec,
        }
    }

    fn update_rows(&mut self, doc: &Doc) {
        let version = if self.right.is_some() { 0 } else { doc.buffer.version() };
        if self.rows_version == Some(version) && !self.rows.is_empty() {
            return;
        }
        let right_text = match &self.right {
            Some(side) => side.buffer.text(),
            None => doc.buffer.text(),
        };
        self.rows = scm::side_by_side(&self.left.buffer.text(), &right_text);
        self.rows_version = Some(version);
    }

    /// Index of the first changed row after (or before) the current scroll position.
    pub fn next_change(&self, forward: bool) -> Option<usize> {
        let top = (self.scroll_y / line_height()).round() as usize;
        let is_change = |r: &DiffRow| !matches!(r, DiffRow::Equal { .. });
        // Start of each run of changed rows.
        let starts: Vec<usize> = (0..self.rows.len())
            .filter(|&i| is_change(&self.rows[i]) && (i == 0 || !is_change(&self.rows[i - 1])))
            .collect();
        if forward {
            starts.iter().copied().find(|&i| i > top + 2).or(starts.first().copied())
        } else {
            starts.iter().rev().copied().find(|&i| i + 2 < top).or(starts.last().copied())
        }
    }

    pub fn scroll_to_row(&mut self, row: usize) {
        let visible = (self.view.h / line_height()).floor();
        self.scroll_y = ((row as f32 - visible / 3.0).max(0.0) * line_height()).min(self.max_scroll());
    }

    fn max_scroll(&self) -> f32 {
        (self.rows.len().saturating_sub(1)) as f32 * line_height()
    }

    pub fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll());
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, 4000.0);
    }

    pub fn draw(&mut self, c: &mut Canvas, theme: &Theme, doc: &mut Doc, view: Rect) {
        self.view = view;
        doc.highlight.update(&mut doc.buffer);
        self.update_rows(doc);
        self.scroll_y = self.scroll_y.min(self.max_scroll());
        c.fill(view, theme.color("editor.background"));

        let style = TextStyle::mono(font_size(), line_height(), theme.color("editor.foreground"));
        let cw = c.measure("0000000000", &style) / 10.0;
        let half = ((view.w - SCROLLBAR_W) / 2.0).floor();
        let left_r = Rect::new(view.x, view.y, half, view.h);
        let right_r = Rect::new(view.x + half + 1.0, view.y, view.w - SCROLLBAR_W - half - 1.0, view.h);

        let first = (self.scroll_y / line_height()).floor() as usize;
        let last = (((self.scroll_y + view.h) / line_height()).ceil() as usize).min(self.rows.len());
        let rows: Vec<DiffRow> = self.rows[first.min(self.rows.len())..last].to_vec();

        // Highlight spans for the visible line range on each side.
        let line_range = |f: &dyn Fn(&DiffRow) -> Option<usize>| {
            let lines: Vec<usize> = rows.iter().filter_map(f).collect();
            (lines.iter().min().copied().unwrap_or(0), lines.iter().max().map_or(0, |m| m + 1))
        };
        let left_of = |r: &DiffRow| match *r {
            DiffRow::Equal { left, .. } | DiffRow::Changed { left, .. } | DiffRow::Removed { left } => Some(left),
            DiffRow::Added { .. } => None,
        };
        let right_of = |r: &DiffRow| match *r {
            DiffRow::Equal { right, .. } | DiffRow::Changed { right, .. } | DiffRow::Added { right } => Some(right),
            DiffRow::Removed { .. } => None,
        };
        let (l0, l1) = line_range(&left_of);
        let (r0, r1) = line_range(&right_of);
        let left_spans = self.left.highlight.spans(&self.left.buffer, l0, l1);
        let right_spans = match &mut self.right {
            Some(side) => side.highlight.spans(&side.buffer, r0, r1),
            None => doc.highlight.spans(&doc.buffer, r0, r1),
        };

        let removed = theme.color("diffEditor.removedLineBackground");
        let inserted = theme.color("diffEditor.insertedLineBackground");
        let filler = theme.color("diffEditor.diagonalFill");
        let digits = self.rows.len().to_string().len().max(3) as f32;
        let gutter_w = 14.0 + digits * cw + 12.0;

        for (side_idx, side_r) in [(0usize, left_r), (1, right_r)] {
            c.push_clip(side_r);
            let text_x = side_r.x + gutter_w - self.scroll_x;
            for (i, row) in rows.iter().enumerate() {
                let y = view.y + (first + i) as f32 * line_height() - self.scroll_y;
                let line = if side_idx == 0 { left_of(row) } else { right_of(row) };
                let row_r = Rect::new(side_r.x, y, side_r.w, line_height());
                match (row, line) {
                    (DiffRow::Equal { .. }, _) => {}
                    (_, None) => {
                        // Filler where the other side has lines: the diagonal hatching.
                        let mut x = side_r.x - line_height();
                        while x < side_r.right() {
                            for k in (0..line_height() as i32).step_by(2) {
                                let px = x + k as f32;
                                if px >= side_r.x && px < side_r.right() {
                                    c.fill(Rect::new(px, y + line_height() - 2.0 - k as f32, 2.0, 2.0), filler);
                                }
                            }
                            x += 10.0;
                        }
                    }
                    _ => c.fill(row_r, if side_idx == 0 { removed } else { inserted }),
                }
                let Some(line) = line else { continue };
                let number = (line + 1).to_string();
                let ns = style.color(theme.color("editorLineNumber.foreground"));
                let nw = c.measure(&number, &ns);
                c.text(side_r.x + 14.0 + digits * cw - nw, y, &number, &ns);
                let (text, spans) = if side_idx == 0 {
                    (self.left.buffer.line(line), left_spans.get(line - l0).cloned().unwrap_or_default())
                } else {
                    let text = match &self.right {
                        Some(side) => side.buffer.line(line),
                        None => doc.buffer.line(line),
                    };
                    (text, right_spans.get(line - r0).cloned().unwrap_or_default())
                };
                let (display, spans): (String, Vec<Span>) = expand_tabs_spans(&text, &spans);
                let colored: Vec<(usize, usize, Color)> = spans.iter().map(|(a, b, t)| (*a, *b, theme.token(*t))).collect();
                c.push_clip(Rect::new(side_r.x + gutter_w - 4.0, side_r.y, side_r.w - gutter_w + 4.0, side_r.h));
                c.rich_text(text_x, y, &display, &colored, &style);
                c.pop_clip();
            }
            c.pop_clip();
        }
        c.fill(Rect::new(view.x + half, view.y, 1.0, view.h), theme.color("diffEditor.border"));

        // Overview ruler with change markers, plus the viewport slider.
        let ruler = Rect::new(view.right() - SCROLLBAR_W, view.y, SCROLLBAR_W, view.h);
        c.fill(Rect::new(ruler.x, ruler.y, 1.0, ruler.h), theme.color("widget.border"));
        let n = self.rows.len().max(1) as f32;
        for (i, row) in self.rows.iter().enumerate() {
            let color = match row {
                DiffRow::Equal { .. } => continue,
                DiffRow::Removed { .. } => theme.color("editorGutter.deletedBackground"),
                DiffRow::Added { .. } => theme.color("editorGutter.addedBackground"),
                DiffRow::Changed { .. } => theme.color("editorGutter.modifiedBackground"),
            };
            let y = ruler.y + i as f32 / n * ruler.h;
            c.fill(Rect::new(ruler.x + 3.0, y, SCROLLBAR_W - 6.0, (ruler.h / n).max(2.0)), color);
        }
        let content = self.max_scroll() + view.h;
        if content > view.h {
            let h = (view.h * view.h / content).max(20.0);
            let top = ruler.y + self.scroll_y / self.max_scroll().max(1.0) * (ruler.h - h);
            c.fill(Rect::new(ruler.x, top, SCROLLBAR_W, h), theme.color("scrollbarSlider.background"));
        }
    }
}
