//! Markdown: Open Preview (⇧⌘V) and Open Preview to the Side (⌘K V): a tab that renders a
//! Markdown document (`crate::markdown`) as it's edited, like the standard preview. Links open
//! (web links in the browser, relative ones as files); fenced code gets syntax colors.
//!
//! Layout happens once per (text version, width) into a list of draw operations; scrolling
//! only moves them.

use std::path::{Path, PathBuf};

use language::{Highlighter, Lang};
use render::{Canvas, Color, Rect, TextStyle};
use text::{Buffer, Selection};

use super::{Focus, Workbench};
use crate::markdown::{parse, Block, Inline};

const BASE: f32 = 14.0;
const LINE: f32 = 22.0;
const PAD: f32 = 26.0;
const MAX_W: f32 = 900.0;

/// How a piece of text looks.
#[derive(Clone, Copy, PartialEq)]
struct Sty {
    size: f32,
    bold: bool,
    italic: bool,
    mono: bool,
    strike: bool,
    color: Color,
}

impl Sty {
    fn style(&self) -> TextStyle {
        let lh = (self.size * 1.6).round();
        if self.mono {
            TextStyle::mono(self.size, lh, self.color)
        } else {
            TextStyle::ui(self.size, self.color).weight(if self.bold { 650 } else { 400 }).italic(self.italic)
        }
    }
}

enum Op {
    Text { x: f32, y: f32, text: String, sty: Sty },
    /// Text with syntax colors (a line of a code block).
    Colored { x: f32, y: f32, text: String, spans: Vec<(usize, usize, Color)>, sty: Sty },
    Fill { r: Rect, color: Color, radius: f32 },
    Check { r: Rect, checked: bool },
}

pub(crate) struct Preview {
    pub scroll: f32,
    /// (text version, width): the layout below is for these.
    key: Option<(u64, f32)>,
    ops: Vec<Op>,
    links: Vec<(Rect, String)>,
    height: f32,
    /// Where the preview was drawn last (for clicks and scrolling).
    pub view: Rect,
    /// An extension's page: the README of this extension, under its header.
    pub extension: Option<String>,
    /// The folder relative links resolve from (the extension's), when the document has no path.
    pub base: Option<PathBuf>,
}

impl Preview {
    pub fn new() -> Self {
        Preview { scroll: 0.0, key: None, ops: Vec::new(), links: Vec::new(), height: 0.0, view: Rect::default(), extension: None, base: None }
    }

    /// The space above the Markdown (an extension page's header).
    fn top(&self) -> f32 {
        if self.extension.is_some() { super::extensions_view::PAGE_HEADER_H } else { 0.0 }
    }

    pub fn scroll_by(&mut self, dy: f32) {
        self.scroll = (self.scroll - dy).clamp(0.0, (self.height + self.top() - self.view.h * 0.5).max(0.0));
    }

    /// The link at window point (x, y).
    pub fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let (cx, cy) = (x - self.view.x - PAD, y - self.view.y + self.scroll - self.top());
        self.links.iter().find(|(r, _)| r.contains(cx, cy)).map(|(_, u)| u.clone())
    }
}

/// The language of a fenced code block's info string ("rust", "py", "sh"...).
fn fence_lang(name: &str) -> Lang {
    let ext = match name.to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "go" | "golang" => "go",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "hpp" => "cpp",
        "js" | "javascript" | "jsx" => "js",
        "ts" | "typescript" | "tsx" => "ts",
        "toml" => "toml",
        "json" | "jsonc" => "json",
        "sh" | "bash" | "zsh" | "shell" | "console" => "sh",
        "md" | "markdown" => "md",
        _ => "txt",
    };
    Lang::detect(Some(Path::new(&format!("x.{ext}"))))
}

impl Workbench {
    /// Markdown: Open Preview (to the side: in the next group).
    pub(super) fn open_markdown_preview(&mut self, side: bool) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let doc = ed.doc;
        let is_md = self.docs[doc].as_ref().is_some_and(|d| d.lang == Lang::Markdown);
        if !is_md {
            return self.set_status_message("The active editor isn't a Markdown file.");
        }
        let target = if side {
            if self.active_group + 1 >= self.groups.len() {
                if self.groups.len() >= 4 {
                    self.active_group
                } else {
                    self.groups.insert(self.active_group + 1, super::Group { tabs: Vec::new(), active: 0, find: Default::default() });
                    self.active_group + 1
                }
            } else {
                self.active_group + 1
            }
        } else {
            self.active_group
        };
        let group = &mut self.groups[target];
        match group.tabs.iter().position(|t| t.doc == doc && t.markdown.is_some()) {
            Some(i) => group.active = i,
            None => {
                let mut ed = crate::editor::EditorState::new(doc);
                ed.markdown = Some(Box::new(Preview::new()));
                let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
                group.tabs.insert(at, ed);
                group.active = at;
            }
        }
        if !side {
            self.active_group = target;
        }
        self.focus = Focus::Editor;
    }

    /// A click in a preview: follows a link under the pointer.
    pub(super) fn click_markdown(&mut self, g: usize, x: f32, y: f32) {
        let Some(ed) = self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)) else { return };
        let (Some(pv), doc) = (ed.markdown.as_ref(), ed.doc) else { return };
        let Some(url) = pv.link_at(x, y) else { return };
        if url.contains("://") || url.starts_with("mailto:") {
            let _ = std::process::Command::new("open").arg(&url).spawn();
            return;
        }
        // A relative link: a file next to the document (an #anchor alone stays here).
        let base = self.docs[doc].as_ref().and_then(|d| d.buffer.path()).and_then(Path::parent).map(Path::to_path_buf).or_else(|| pv.base.clone());
        let file = url.split('#').next().unwrap_or_default();
        if let (Some(base), false) = (base, file.is_empty()) {
            let path: PathBuf = base.join(file);
            if path.is_file() {
                self.open_file(&path);
            }
        }
    }

    /// Draws the preview of group `g`'s active tab in `r`.
    pub(super) fn draw_markdown(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let Some(pv) = ed.markdown.as_mut() else { return };
        let theme = &self.theme;
        c.fill(r, theme.color("editor.background"));
        pv.view = r;
        let width = (r.w - PAD * 2.0).min(MAX_W);
        let key = (doc.buffer.version(), width);
        if pv.key != Some(key) {
            let blocks = parse(&doc.buffer.text());
            let fg = theme.color("editor.foreground");
            let (ops, links, height) = layout(c, theme, fg, &blocks, width);
            pv.ops = ops;
            pv.links = links;
            pv.height = height;
            pv.key = Some(key);
        }
        let top = pv.top();
        pv.scroll = pv.scroll.clamp(0.0, (pv.height + top - r.h * 0.5).max(0.0));
        let (ox, oy) = (r.x + PAD, r.y - pv.scroll + top);
        let header = pv.extension.clone().map(|id| (id, r.y - pv.scroll));
        c.push_clip(r);
        for op in &pv.ops {
            match op {
                Op::Fill { r: fr, color, radius } => {
                    let fr = Rect::new(fr.x + ox, fr.y + oy, fr.w, fr.h);
                    if fr.bottom() >= r.y && fr.y <= r.bottom() {
                        c.fill_rounded(fr, *color, *radius);
                    }
                }
                Op::Text { x, y, text, sty } => {
                    let (tx, ty) = (x + ox, y + oy);
                    if ty + LINE * 2.0 < r.y || ty > r.bottom() {
                        continue;
                    }
                    let st = sty.style();
                    let w = c.text(tx, ty, text, &st);
                    if sty.strike {
                        c.fill(Rect::new(tx, ty + st.line_height / 2.0, w, 1.0), sty.color);
                    }
                }
                Op::Colored { x, y, text, spans, sty } => {
                    let (tx, ty) = (x + ox, y + oy);
                    if ty + LINE < r.y || ty > r.bottom() {
                        continue;
                    }
                    c.rich_text(tx, ty, text, spans, &sty.style());
                }
                Op::Check { r: cr, checked } => {
                    let cr = Rect::new(cr.x + ox, cr.y + oy, cr.w, cr.h);
                    c.bordered(cr, theme.color("checkbox.background"), theme.color("checkbox.border"), 1.0, 3.0);
                    if *checked {
                        c.icon_in(&crate::icons::CHECK, cr, cr.w - 2.0, theme.color("checkbox.foreground"));
                    }
                }
            }
        }
        c.pop_clip();
        self.hits.push((r, super::Hit::Markdown(g)));
        if let Some((id, y)) = header {
            c.push_clip(r);
            self.draw_extension_page_header(c, g, &id, r, y);
            c.pop_clip();
        }
    }
}

/// Lays out `blocks` in `width`: (draw operations, links, total height). Coordinates are
/// relative to the content's top-left corner.
fn layout(c: &mut Canvas, theme: &theme::Theme, fg: Color, blocks: &[Block], width: f32) -> (Vec<Op>, Vec<(Rect, String)>, f32) {
    let mut ops = Vec::new();
    let mut links = Vec::new();
    let mut y = 16.0;
    let base = Sty { size: BASE, bold: false, italic: false, mono: false, strike: false, color: fg };
    blocks_into(c, theme, base, blocks, 0.0, width, &mut y, &mut ops, &mut links);
    (ops, links, y + 40.0)
}

#[allow(clippy::too_many_arguments)]
fn blocks_into(c: &mut Canvas, theme: &theme::Theme, base: Sty, blocks: &[Block], x: f32, width: f32, y: &mut f32, ops: &mut Vec<Op>, links: &mut Vec<(Rect, String)>) {
    for (bi, block) in blocks.iter().enumerate() {
        let last = bi + 1 == blocks.len();
        match block {
            Block::Heading(level, inl) => {
                let scale = [2.0, 1.5, 1.25, 1.0, 0.875, 0.85][(*level as usize - 1).min(5)];
                let sty = Sty { size: BASE * scale, bold: true, ..base };
                *y += if *level <= 2 { 8.0 } else { 6.0 };
                inline_flow(c, theme, sty, inl, x, width, y, ops, links);
                if *level <= 2 {
                    *y += 6.0;
                    ops.push(Op::Fill { r: Rect::new(x, *y, width, 1.0), color: theme.color("textSeparator.foreground"), radius: 0.0 });
                    *y += 1.0;
                }
                *y += 12.0;
            }
            Block::Paragraph(inl) => {
                inline_flow(c, theme, base, inl, x, width, y, ops, links);
                *y += if last { 0.0 } else { 14.0 };
            }
            Block::Rule => {
                *y += 8.0;
                ops.push(Op::Fill { r: Rect::new(x, *y, width, 2.0), color: theme.color("textSeparator.foreground"), radius: 0.0 });
                *y += 20.0;
            }
            Block::Code { lang, lines } => {
                let sty = Sty { size: 13.0, mono: true, ..base };
                let lh = (sty.size * 1.6).round();
                let h = lines.len() as f32 * lh + 24.0;
                ops.push(Op::Fill { r: Rect::new(x, *y, width, h), color: theme.color("textCodeBlock.background"), radius: 4.0 });
                let lang = fence_lang(lang);
                let mut buffer = Buffer::new();
                buffer.insert(Selection::default(), &lines.join("\n"));
                let mut hl = Highlighter::new(lang);
                hl.update(&mut buffer);
                let spans = hl.spans(&buffer, 0, lines.len());
                for (i, line) in lines.iter().enumerate() {
                    let (text, sp) = crate::editor::expand_tabs_spans(line, spans.get(i).map(Vec::as_slice).unwrap_or_default());
                    let colored = sp.iter().map(|(a, z, t)| (*a, *z, theme.token(*t))).collect();
                    ops.push(Op::Colored { x: x + 16.0, y: *y + 12.0 + i as f32 * lh, text, spans: colored, sty });
                }
                *y += h + if last { 0.0 } else { 16.0 };
            }
            Block::Quote(inner) => {
                // The background and bar go first (sized once the quote's text is laid out).
                let top = *y;
                let bg = ops.len();
                ops.push(Op::Fill { r: Rect::default(), color: theme.color("textBlockQuote.background"), radius: 0.0 });
                ops.push(Op::Fill { r: Rect::default(), color: theme.color("textBlockQuote.border"), radius: 0.0 });
                let mut iy = *y + 4.0;
                let quote = Sty { color: base.color.with_alpha(0.8), ..base };
                blocks_into(c, theme, quote, inner, x + 16.0, width - 16.0, &mut iy, ops, links);
                iy += 4.0;
                if let Op::Fill { r, .. } = &mut ops[bg] {
                    *r = Rect::new(x, top, width, iy - top);
                }
                if let Op::Fill { r, .. } = &mut ops[bg + 1] {
                    *r = Rect::new(x, top, 4.0, iy - top);
                }
                *y = iy + if last { 0.0 } else { 14.0 };
            }
            Block::List { start, items } => {
                for (k, item) in items.iter().enumerate() {
                    let marker_y = *y;
                    let content_x = x + 26.0;
                    match (item.task, start) {
                        (Some(checked), _) => ops.push(Op::Check { r: Rect::new(content_x - 20.0, marker_y + 4.0, 14.0, 14.0), checked }),
                        (None, Some(n)) => ops.push(Op::Text { x: x + 4.0, y: marker_y, text: format!("{}.", n + k as u64), sty: base }),
                        (None, None) => ops.push(Op::Text { x: x + 10.0, y: marker_y, text: "•".into(), sty: base }),
                    }
                    blocks_into(c, theme, base, &item.blocks, content_x, width - 26.0, y, ops, links);
                    *y += 4.0;
                }
                *y += if last { 0.0 } else { 10.0 };
            }
            Block::Table(header, rows) => {
                let cols = header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0)).max(1);
                let col_w = width / cols as f32;
                let border = theme.color("textSeparator.foreground");
                let row_into = |c: &mut Canvas, cells: &[Vec<Inline>], bold: bool, y: &mut f32, ops: &mut Vec<Op>, links: &mut Vec<(Rect, String)>| {
                    let top = *y;
                    let mut bottom = top;
                    for (i, cell) in cells.iter().enumerate().take(cols) {
                        let mut cy = top + 6.0;
                        inline_flow(c, theme, Sty { bold, ..base }, cell, x + i as f32 * col_w + 10.0, col_w - 20.0, &mut cy, ops, links);
                        bottom = bottom.max(cy + 6.0);
                    }
                    ops.push(Op::Fill { r: Rect::new(x, bottom, width, 1.0), color: border, radius: 0.0 });
                    *y = bottom + 1.0;
                };
                row_into(c, header, true, y, ops, links);
                for row in rows {
                    row_into(c, row, false, y, ops, links);
                }
                *y += if last { 0.0 } else { 14.0 };
            }
        }
    }
}

/// Flows inline runs into lines of `width` from `x`, word by word. Advances `y` past them.
#[allow(clippy::too_many_arguments)]
fn inline_flow(c: &mut Canvas, theme: &theme::Theme, base: Sty, runs: &[Inline], x: f32, width: f32, y: &mut f32, ops: &mut Vec<Op>, links: &mut Vec<(Rect, String)>) {
    let line_h = (base.size * 1.6).round().max(LINE);
    let mut cx = x;
    for run in runs {
        let mut sty = Sty { bold: base.bold || run.bold, italic: base.italic || run.italic, strike: run.strike, ..base };
        if run.link.is_some() {
            sty.color = theme.color("textLink.foreground");
        }
        let text = if run.image { format!("[image: {}]", run.text) } else { run.text.clone() };
        if run.code {
            sty.mono = true;
            sty.size = base.size * 0.9;
            sty.color = theme.color("textPreformat.foreground");
        }
        // Words keep their trailing space; a code span stays in one piece where it fits.
        let pieces: Vec<String> = if run.code {
            vec![text]
        } else {
            let mut v = Vec::new();
            let mut cur = String::new();
            for ch in text.chars() {
                cur.push(ch);
                if ch == ' ' {
                    v.push(std::mem::take(&mut cur));
                }
            }
            if !cur.is_empty() {
                v.push(cur);
            }
            v
        };
        for piece in pieces {
            let st = sty.style();
            let w = c.measure(&piece, &st);
            if cx > x && cx + w.min(width) > x + width + 0.5 && !piece.trim().is_empty() {
                cx = x;
                *y += line_h;
            }
            let ty = *y + ((line_h - st.line_height) / 2.0).round();
            if run.code {
                ops.push(Op::Fill { r: Rect::new(cx - 2.0, *y + 3.0, w + 4.0, line_h - 6.0), color: theme.color("textPreformat.background"), radius: 3.0 });
            }
            if let Some(url) = &run.link {
                links.push((Rect::new(cx, *y, w, line_h), url.clone()));
            }
            ops.push(Op::Text { x: cx, y: ty, text: piece, sty });
            cx += w;
        }
    }
    *y += line_h;
}
