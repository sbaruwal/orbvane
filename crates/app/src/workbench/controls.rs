//! The controls every view shares, drawn one way: text fields, buttons, count badges, empty
//! states, cards, segmented controls and switches. Rounded like the toolbar's search field
//! and the chips, with colors from the theme's standard keys.

use render::{Canvas, Color, Icon, Rect, TextStyle};

use super::{Hit, Workbench, UI};

/// Text fields and buttons.
pub(super) const FIELD_RADIUS: f32 = 6.0;
/// Cards, floating widgets and the panel.
pub(super) const CARD_RADIUS: f32 = 10.0;
/// Hovers, suggestions, the rename box and other popups over the editor.
pub(super) const POPUP_RADIUS: f32 = 8.0;
/// The height of a text field or button in a side bar view.
pub(super) const FIELD_H: f32 = 28.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ButtonKind {
    /// The view's main action, filled with the accent.
    Primary,
    /// Everything else: a quiet tint.
    Secondary,
}

impl Workbench {
    /// A text field's frame (its text is drawn by `TextField::draw` inside `field_text_rect`):
    /// rounded, with the focus color as its border while it has the keyboard.
    pub(super) fn field_frame(&self, c: &mut Canvas, r: Rect, focused: bool) {
        let border = if focused { self.color("focusBorder") } else { self.color_or("input.border", "widget.border") };
        c.bordered(r, self.color("input.background"), border, 1.0, FIELD_RADIUS);
    }

    /// Where a field's text goes: inset from its frame.
    pub(super) fn field_text_rect(r: Rect) -> Rect {
        Rect::new(r.x + 9.0, r.y, (r.w - 18.0).max(0.0), r.h)
    }

    /// A button with a centered label.
    pub(super) fn button(&mut self, c: &mut Canvas, r: Rect, label: &str, kind: ButtonKind, enabled: bool, hit: Hit) {
        let hovered = enabled && self.hovered(hit);
        let (bg, fg) = match kind {
            _ if !enabled => (self.color("input.background"), self.color("disabledForeground")),
            ButtonKind::Primary => {
                (self.color(if hovered { "button.hoverBackground" } else { "button.background" }), self.color("button.foreground"))
            }
            ButtonKind::Secondary => (
                self.color(if hovered { "button.secondaryHoverBackground" } else { "button.secondaryBackground" }),
                self.color("button.secondaryForeground"),
            ),
        };
        c.fill_rounded(r, bg, FIELD_RADIUS);
        let st = TextStyle::ui(UI, fg).weight(500);
        let lw = c.measure(label, &st).min(r.w - 12.0);
        c.text_fit(Rect::new(r.x + ((r.w - lw) / 2.0).max(6.0), r.y, lw + 2.0, r.h), label, &st);
        if enabled {
            self.hits.push((r, hit));
        }
    }

    /// What a view or panel shows with nothing in it: an icon, a line (and a second, dimmer
    /// one), and optionally a button, centered in `r` (or from its top when `r` is tall).
    /// Returns the bottom of what was drawn.
    pub(super) fn empty_state(&mut self, c: &mut Canvas, r: Rect, icon: &Icon, title: &str, detail: &str, action: Option<(&str, Hit)>) -> f32 {
        let fg = self.color("foreground");
        let dim = self.color("descriptionForeground");
        let width = (r.w - 32.0).clamp(0.0, 320.0);
        let title_st = TextStyle::ui(UI, fg).weight(600);
        let detail_st = TextStyle::ui(UI, dim);
        let title_lines = crate::widgets::wrap(c, title, &title_st, width);
        let detail_lines = if detail.is_empty() { Vec::new() } else { crate::widgets::wrap(c, detail, &detail_st, width) };
        let height = 40.0 + 18.0 * title_lines.len() as f32 + 6.0 + 18.0 * detail_lines.len() as f32 + if action.is_some() { 44.0 } else { 0.0 };
        // Centered in short areas; a third of the way down in tall ones.
        let top = r.y + ((r.h - height) / 2.0).clamp(12.0, 120.0);
        let cx = r.x + r.w / 2.0;
        c.icon(icon, cx - 14.0, top, 28.0, dim);
        let mut y = top + 40.0;
        for line in &title_lines {
            let w = c.measure(line, &title_st);
            c.text(cx - w / 2.0, y, line, &title_st);
            y += 18.0;
        }
        y += 6.0;
        for line in &detail_lines {
            let w = c.measure(line, &detail_st);
            c.text(cx - w / 2.0, y, line, &detail_st);
            y += 18.0;
        }
        if let Some((label, hit)) = action {
            let st = TextStyle::ui(UI, fg).weight(500);
            let bw = (c.measure(label, &st) + 32.0).min(r.w - 24.0);
            let b = Rect::new(cx - bw / 2.0, y + 12.0, bw, FIELD_H);
            self.button(c, b, label, ButtonKind::Primary, true, hit);
            y = b.bottom();
        }
        y
    }

    /// A raised card (settings groups, the Assistant's agents, the palette).
    pub(super) fn card(&self, c: &mut Canvas, r: Rect) {
        c.bordered(r, self.color("editorWidget.background"), self.color("widget.border"), 1.0, CARD_RADIUS);
    }

    /// A floating widget (palette, find, hovers, suggestions, notifications): a card with a
    /// shadow, on a layer of its own.
    pub(super) fn floating(&self, c: &mut Canvas, r: Rect, radius: f32) {
        c.shadow(r, radius, self.color("widget.shadow"));
        let border = self.color_or("editorWidget.border", "widget.border");
        c.bordered(r, self.color("editorWidget.background"), border, 1.0, radius);
    }

    /// A segmented control: `labels` side by side in a rounded track, `active` raised. Hits
    /// come from `hit(i)`. Returns its width.
    pub(super) fn segmented(&mut self, c: &mut Canvas, x: f32, y: f32, h: f32, labels: &[&str], active: usize, hit: impl Fn(usize) -> Hit) -> f32 {
        let fg = self.color("foreground");
        let dim = self.color("descriptionForeground");
        let st = TextStyle::ui(UI, fg);
        let widths: Vec<f32> = labels.iter().map(|l| c.measure(l, &st) + 24.0).collect();
        let total = widths.iter().sum::<f32>() + 4.0;
        let track = Rect::new(x, y, total, h);
        c.fill_rounded(track, self.color("input.background"), FIELD_RADIUS + 1.0);
        let mut sx = x + 2.0;
        for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
            let seg = Rect::new(sx, y + 2.0, *w, h - 4.0);
            let on = i == active;
            if on {
                c.bordered(seg, self.color("button.secondaryBackground"), self.color("widget.border"), 1.0, FIELD_RADIUS);
            } else if self.hovered(hit(i)) {
                c.fill_rounded(seg, self.color("toolbar.hoverBackground"), FIELD_RADIUS);
            }
            let st = TextStyle::ui(UI, if on { fg } else { dim }).weight(if on { 600 } else { 400 });
            let tw = c.measure(label, &st);
            c.text_in(Rect::new(seg.x + (seg.w - tw) / 2.0, seg.y, tw + 1.0, seg.h), label, &st);
            self.hits.push((seg, hit(i)));
            sx += w;
        }
        total
    }

    /// An on/off switch at (x, y), 32×18.
    pub(super) fn switch(&mut self, c: &mut Canvas, x: f32, y: f32, on: bool, hit: Hit) -> Rect {
        let r = Rect::new(x, y, 32.0, 18.0);
        let track = if on { self.color("button.background") } else { self.color_or("input.border", "widget.border") };
        c.fill_rounded(r, track, 9.0);
        let knob_x = if on { r.right() - 16.0 } else { r.x + 2.0 };
        let knob: Color = self.color("button.foreground");
        c.fill_rounded(Rect::new(knob_x, r.y + 2.0, 14.0, 14.0), knob, 7.0);
        self.hits.push((r, hit));
        r
    }
}
