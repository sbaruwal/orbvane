//! Signature help: while typing a call, a popup above the cursor
//! shows the function's signature with the current parameter highlighted, and its docs.
//! Opened by the server's trigger characters (`(`, `,`) or ⇧⌘Space; while open it follows the
//! cursor and edits, and closes when the server has nothing to show.

use std::path::Path;

use render::{Canvas, Rect, TextStyle};
use serde_json::json;
use text::Pos;

use super::intel::{draw_rows, layout_blocks, parse_markdown, row_height, row_width, Block};
use super::{Focus, Hit, Workbench, SMALL, UI};
use crate::editor::{font_size, line_height};
use crate::input::{Key, KeyInput};

const MAX_W: f32 = 500.0;
const MAX_H: f32 = 250.0;

#[derive(Default)]
pub(super) struct SignatureState {
    /// The latest request, and where the cursor was for it: (group, doc, head, version).
    seq: u64,
    asked_at: Option<(usize, usize, Pos, u64)>,
    help: Option<lsp::SignatureHelp>,
    /// A signature picked with ↑/↓ (kept while the server offers as many).
    picked: Option<usize>,
}

impl Workbench {
    fn signature_visible(&self) -> bool {
        self.signature.help.is_some()
    }

    pub(super) fn hide_signature(&mut self) {
        self.signature.help = None;
        self.signature.asked_at = None;
        self.signature.picked = None;
    }

    /// Asks for signature help at the cursor. `trigger`: the typed trigger character, if any.
    pub(super) fn trigger_signature_help(&mut self, trigger: Option<&str>) {
        if !self.settings.bool("editor.parameterHints.enabled") {
            return;
        }
        let g = self.active_group;
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let (doc_id, head) = (ed.doc, ed.sel.head);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        if !self.lsp.supports(&path, "signatureHelpProvider") {
            return;
        }
        let retrigger = self.signature_visible();
        // 1: invoked, 2: trigger character, 3: the cursor moved or the text changed.
        let kind = match trigger {
            Some(_) => 2,
            None if retrigger => 3,
            None => 1,
        };
        let mut context = json!({ "triggerKind": kind, "isRetrigger": retrigger });
        if let Some(t) = trigger {
            context["triggerCharacter"] = json!(t);
        }
        let s = &mut self.signature;
        s.seq += 1;
        s.asked_at = Some((g, doc_id, head, doc.buffer.version()));
        let doc = self.docs[doc_id].as_ref().unwrap();
        self.lsp.signature_help(&path, &doc.buffer, head, context, self.signature.seq);
    }

    /// After a typed character: open (or update) on the server's trigger characters.
    pub(super) fn signature_after_typing(&mut self, typed: &str) {
        let Some(path) = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        let (triggers, retriggers) = self.lsp.signature_triggers(&path);
        if triggers.iter().any(|t| t == typed) || (self.signature_visible() && retriggers.iter().any(|t| t == typed)) {
            self.trigger_signature_help(Some(typed));
        }
    }

    /// While open: follows the cursor and edits (from keys, clicks or anything else), and
    /// closes when the editor loses focus. Called every frame.
    pub(super) fn signature_tick(&mut self) {
        let Some((g, doc, head, version)) = self.signature.asked_at else { return };
        if !self.signature_visible() {
            return;
        }
        let here = self.active_editor().filter(|_| self.focus == Focus::Editor && self.palette.is_none()).map(|e| (e.doc, e.sel.head));
        let Some((cur_doc, cur_head)) = here.filter(|_| self.active_group == g) else { return self.hide_signature() };
        if cur_doc != doc {
            return self.hide_signature();
        }
        let cur_version = self.docs[doc].as_ref().map_or(0, |d| d.buffer.version());
        if (cur_head, cur_version) != (head, version) {
            self.trigger_signature_help(None);
        }
    }

    pub(super) fn signature_help_arrived(&mut self, seq: u64, help: Option<lsp::SignatureHelp>) {
        let s = &mut self.signature;
        if seq != s.seq {
            return;
        }
        match help {
            None => self.hide_signature(),
            Some(h) => {
                if s.picked.is_some_and(|p| p >= h.signatures.len()) {
                    s.picked = None;
                }
                s.help = Some(h);
            }
        }
    }

    /// Escape closes it; ↑/↓ go through overloads. Returns true if the key was used.
    pub(super) fn signature_key(&mut self, k: &KeyInput) -> bool {
        let Some(help) = &self.signature.help else { return false };
        let n = help.signatures.len();
        match k.key {
            Key::Escape => {
                self.hide_signature();
                true
            }
            Key::Up | Key::Down if n > 1 && !k.shift && !k.cmd && !k.alt => {
                self.cycle_signature(if k.key == Key::Up { -1 } else { 1 });
                true
            }
            _ => false,
        }
    }

    pub(super) fn cycle_signature(&mut self, d: isize) {
        let Some(help) = &self.signature.help else { return };
        let n = help.signatures.len() as isize;
        let cur = self.signature.picked.unwrap_or(help.active_signature) as isize;
        self.signature.picked = Some((cur + d).rem_euclid(n) as usize);
    }

    pub(super) fn draw_signature(&mut self, c: &mut Canvas) {
        let Some(help) = &self.signature.help else { return };
        let Some((g, doc_id, _, _)) = self.signature.asked_at else { return };
        let Some(ed) = self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)).filter(|e| e.doc == doc_id) else { return };
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let index = self.signature.picked.unwrap_or(help.active_signature).min(help.signatures.len() - 1);
        let sig = &help.signatures[index];
        let active = sig.active_parameter.or(help.active_parameter);
        let param = active.and_then(|i| sig.parameters.get(i));

        let fg = self.color("editorHoverWidget.foreground");
        let highlight = self.color_or("editorHoverWidget.highlightForeground", "list.highlightForeground");
        let text_style = TextStyle::ui(UI, fg);
        let code_style = TextStyle::mono(font_size(), line_height(), fg);
        let bold = code_style.color(highlight).weight(700);
        let pad = 8.0;
        let max_w = MAX_W.min(self.main_rect.w - 40.0);
        let multi = help.signatures.len() > 1;
        let counter = format!("{}/{}", index + 1, help.signatures.len());
        let counter_style = TextStyle::ui(SMALL, fg);
        let lead = if multi { 16.0 + c.measure(&counter, &counter_style) + 16.0 + 6.0 } else { 0.0 };
        let inner_w = max_w - pad * 2.0;

        // The signature, wrapped by characters (monospace), in segments around the parameter.
        let cw = c.measure("0", &code_style).max(1.0);
        let per_line = ((inner_w - lead) / cw).floor().max(10.0) as usize;
        let chars: Vec<(usize, char)> = sig.label.char_indices().collect();
        let lines: Vec<(usize, usize)> = chars
            .chunks(per_line)
            .map(|ch| (ch[0].0, ch.last().map_or(0, |(i, c)| i + c.len_utf8())))
            .collect();

        // Docs: the parameter's, then the signature's.
        let mut blocks: Vec<Block> = Vec::new();
        if let Some((_, pdoc)) = param.filter(|(_, d)| !d.trim().is_empty()) {
            blocks.extend(parse_markdown(pdoc));
        }
        if !sig.documentation.trim().is_empty() {
            if !blocks.is_empty() {
                blocks.push(Block::Rule);
            }
            blocks.extend(parse_markdown(&sig.documentation));
        }
        let rows = layout_blocks(c, &blocks, &text_style, inner_w);
        let label_w = lines.iter().map(|(a, b)| crate::editor::display_width(&sig.label[*a..*b]) as f32 * cw).fold(0.0f32, f32::max) + lead;
        let docs_w = rows.iter().map(|r| row_width(c, r, &text_style, &code_style)).fold(0.0f32, f32::max);
        let docs_h: f32 = rows.iter().map(|r| row_height(r, &text_style)).sum();
        let label_h = lines.len() as f32 * line_height();
        let sep = if rows.is_empty() { 0.0 } else { 9.0 };
        let w = label_w.max(docs_w).min(inner_w) + pad * 2.0;
        let h = (label_h + sep + docs_h + pad * 2.0).min(MAX_H);

        // Above the cursor line if there's room, otherwise below it.
        let (cx, cy) = ed.point_of(doc, ed.sel.head);
        let view = ed.geom.text;
        let y = if cy - h - 4.0 >= view.y { cy - h - 4.0 } else { cy + line_height() + 4.0 };
        let x = cx.min(self.main_rect.right() - w - 8.0).max(self.main_rect.x + 4.0);
        let r = Rect::new(x.round(), y.round(), w.round(), h.round());
        c.shadow(r, super::controls::POPUP_RADIUS, self.color("widget.shadow"));
        c.bordered(r, self.color("editorHoverWidget.background"), self.color("editorHoverWidget.border"), 1.0, super::controls::POPUP_RADIUS);
        c.push_clip(r.inset(1.0, 1.0));
        let mut hits = vec![(r, Hit::SignatureHelp)];
        if multi {
            let icon_y = r.y + pad + (line_height() - 16.0) / 2.0;
            let prev = Rect::new(r.x + pad - 2.0, icon_y, 16.0, 16.0);
            c.icon(&crate::icons::CHEVRON_UP, prev.x, prev.y, 16.0, fg);
            let tw = c.text_in(Rect::new(prev.right() + 2.0, r.y + pad, 60.0, line_height()), &counter, &counter_style);
            let next = Rect::new(prev.right() + 4.0 + tw, icon_y, 16.0, 16.0);
            c.icon(&crate::icons::CHEVRON_DOWN, next.x, next.y, 16.0, fg);
            hits.push((prev, Hit::SignatureCycle(false)));
            hits.push((next, Hit::SignatureCycle(true)));
        }
        let prange = param.map(|(range, _)| range.clone()).unwrap_or(0..0);
        for (i, (a, b)) in lines.iter().enumerate() {
            let y = r.y + pad + i as f32 * line_height();
            let mut x = r.x + pad + lead;
            // Before, inside and after the parameter, clipped to this line.
            let cuts = [*a, prange.start.clamp(*a, *b), prange.end.clamp(*a, *b), *b];
            for (j, win) in cuts.windows(2).enumerate() {
                if win[0] >= win[1] {
                    continue;
                }
                let st = if j == 1 { &bold } else { &code_style };
                let part = &sig.label[win[0]..win[1]];
                c.text(x, y, part, st);
                if j == 1 {
                    c.fill(Rect::new(x, y + line_height() - 2.0, crate::editor::display_width(part) as f32 * cw, 1.0), highlight);
                }
                x += part.chars().count() as f32 * cw;
            }
        }
        if !rows.is_empty() {
            let rule_y = r.y + pad + label_h;
            c.fill(Rect::new(r.x, rule_y + 4.0, r.w, 1.0), self.color("editorHoverWidget.border"));
            draw_rows(c, &self.theme, &rows, r, rule_y + sep, pad, &text_style, &code_style);
        }
        c.pop_clip();
        self.hits.extend(hits);
    }
}
