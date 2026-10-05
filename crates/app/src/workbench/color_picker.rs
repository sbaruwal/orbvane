//! The color picker, opened by clicking a color swatch: a
//! header with the picked color and its text (click to switch between rgb, hsl and hex) next to
//! the original color (click to go back to it), over a saturation/brightness box, an opacity
//! strip and a hue strip. Letting go of what was dragged writes the color into the document.

use std::sync::Arc;

use render::{Canvas, Image, Rect, TextStyle};
use text::Pos;

use super::{Drag, Hit, Workbench};

const HEADER_H: f32 = 24.0;
const PAD: f32 = 8.0;
const SAT_W: f32 = 220.0;
const SAT_H: f32 = 150.0;
const STRIP_W: f32 = 25.0;
const ORIGINAL_W: f32 = 74.0;
const WIDTH: f32 = PAD + SAT_W + PAD + STRIP_W + PAD + STRIP_W + PAD;
const HEIGHT: f32 = HEADER_H + PAD + SAT_H + PAD;

/// The draggable parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Part {
    Saturation,
    Opacity,
    Hue,
}

/// A color as 0..255 channels and alpha 0..1, like `RGBA`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: f64,
}

/// Hue (whole degrees), saturation, value and alpha, rounded like `HSVA`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hsva {
    h: f64,
    s: f64,
    v: f64,
    a: f64,
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

impl Hsva {
    fn new(h: f64, s: f64, v: f64, a: f64) -> Self {
        Self { h: h.clamp(0.0, 360.0).trunc(), s: round3(s.clamp(0.0, 1.0)), v: round3(v.clamp(0.0, 1.0)), a: round3(a.clamp(0.0, 1.0)) }
    }

    fn from_rgba(c: Rgba) -> Self {
        let (r, g, b) = (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
        let max = r.max(g).max(b);
        let delta = max - r.min(g).min(b);
        let s = if max == 0.0 { 0.0 } else { delta / max };
        let m = if delta == 0.0 {
            0.0
        } else if max == r {
            (((g - b) / delta) % 6.0 + 6.0) % 6.0
        } else if max == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
        Self::new((m * 60.0).round(), s, max, c.a)
    }

    fn to_rgba(self) -> Rgba {
        let Hsva { h, s, v, a } = self;
        let c = v * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = v - c;
        let (r, g, b) = match h {
            h if h < 60.0 => (c, x, 0.0),
            h if h < 120.0 => (x, c, 0.0),
            h if h < 180.0 => (0.0, c, x),
            h if h < 240.0 => (0.0, x, c),
            h if h < 300.0 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let ch = |v: f64| ((v + m) * 255.0).round() as u8;
        Rgba { r: ch(r), g: ch(g), b: ch(b), a }
    }
}

impl Rgba {
    pub(crate) fn from_color(c: theme::Color) -> Self {
        let ch = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        Self { r: ch(c.r), g: ch(c.g), b: ch(c.b), a: c.a as f64 }
    }

    fn color(self) -> theme::Color {
        theme::Color { a: self.a as f32, ..theme::Color::rgba8(self.r, self.g, self.b, 255) }
    }

    /// Hue (whole degrees), saturation and lightness, like `HSLA.fromRGBA`.
    fn hsl(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r as f64 / 255.0, self.g as f64 / 255.0, self.b as f64 / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let l = (max + min) / 2.0;
        let chroma = max - min;
        if chroma <= 0.0 {
            return (0.0, 0.0, l);
        }
        let s = (if l <= 0.5 { chroma / (2.0 * l) } else { chroma / (2.0 - 2.0 * l) }).min(1.0);
        let h = if max == r {
            (g - b) / chroma + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / chroma + 2.0
        } else {
            (r - g) / chroma + 4.0
        };
        ((h * 60.0).round(), s, l)
    }

    /// Whether text on it should be dark.
    fn is_lighter(self) -> bool {
        (self.r as f64 * 299.0 + self.g as f64 * 587.0 + self.b as f64 * 114.0) / 1000.0 >= 128.0
    }
}

/// A number as JavaScript prints `+x.toFixed(2)` ("0.5", "1").
fn short2(x: f64) -> String {
    let s = format!("{x:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".into() } else { s.into() }
}

/// The default color presentations: `rgb()`/`rgba()`, `hsl()`/`hsla()` and hex.
pub(crate) fn presentations(c: Rgba) -> [String; 3] {
    let rgb = if c.a == 1.0 { format!("rgb({}, {}, {})", c.r, c.g, c.b) } else { format!("rgba({}, {}, {}, {})", c.r, c.g, c.b, short2(c.a)) };
    let (h, s, l) = c.hsl();
    let (s, l) = ((s * 100.0).round(), (l * 100.0).round());
    let hsl = if c.a == 1.0 { format!("hsl({h}, {s}%, {l}%)") } else { format!("hsla({h}, {s}%, {l}%, {:.2})", c.a) };
    let hex = if c.a == 1.0 {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, (c.a * 255.0).round() as u8)
    };
    [rgb, hsl, hex]
}

/// Which presentation the color's text is written in.
fn guess_presentation(c: Rgba, text: &str) -> usize {
    let labels = presentations(c);
    let text = text.to_lowercase();
    if let Some(i) = labels.iter().position(|l| *l == text) {
        return i;
    }
    let prefix = text.split('(').next().unwrap_or("");
    labels.iter().position(|l| l.to_lowercase().starts_with(prefix)).unwrap_or(0)
}

pub(super) struct ColorPicker {
    g: usize,
    doc: usize,
    /// The color's text: its line and char columns.
    line: usize,
    start: usize,
    end: usize,
    original: Rgba,
    color: Hsva,
    /// What the document has (the last color written).
    written: Rgba,
    presentation: usize,
    /// The buffer version after our last edit; edits from elsewhere close the picker.
    version: u64,
    /// Where the picker is (set when drawn).
    rect: Rect,
    /// The saturation box for a hue, and the opacity strip for a color.
    saturation: Option<(f64, Arc<Image>)>,
    opacity: Option<((u8, u8, u8), Arc<Image>)>,
    hue: Arc<Image>,
}

/// An image of `w`×`h` pixels from a function of (x, y) in 0..1.
fn image(w: u32, h: u32, f: impl Fn(f64, f64) -> [u8; 4]) -> Arc<Image> {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend(f((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64));
        }
    }
    Arc::new(Image::new(w, h, rgba))
}

/// Draws the transparency checkerboard (`opacity-background.png`: #bbb and #444 at
/// 10% alpha, 4.5px squares) over `r`.
fn checkerboard(c: &mut Canvas, r: Rect) {
    const CELL: f32 = 4.5;
    let (light, dark) = (theme::Color::rgba8(0xbb, 0xbb, 0xbb, 0x19), theme::Color::rgba8(0x44, 0x44, 0x44, 0x19));
    c.push_clip(r);
    let (cols, rows) = ((r.w / CELL).ceil() as usize, (r.h / CELL).ceil() as usize);
    for j in 0..rows {
        for i in 0..cols {
            let color = if (i + j) % 2 == 0 { light } else { dark };
            c.fill(Rect::new(r.x + i as f32 * CELL, r.y + j as f32 * CELL, CELL, CELL), color);
        }
    }
    c.pop_clip();
}

impl ColorPicker {
    fn rgba(&self) -> Rgba {
        self.color.to_rgba()
    }

    fn label(&self) -> String {
        presentations(self.rgba())[self.presentation].clone()
    }

    fn parts(&self) -> (Rect, Rect, Rect, Rect, Rect) {
        let r = self.rect;
        let header = Rect::new(r.x + 1.0, r.y + 1.0, WIDTH, HEADER_H);
        let picked = Rect::new(header.x, header.y, WIDTH - ORIGINAL_W, HEADER_H);
        let original = Rect::new(picked.right(), header.y, ORIGINAL_W, HEADER_H);
        let top = header.bottom() + PAD;
        let sat = Rect::new(r.x + 1.0 + PAD, top, SAT_W, SAT_H);
        let opacity = Rect::new(sat.right() + PAD, top, STRIP_W, SAT_H);
        let hue = Rect::new(opacity.right() + PAD, top, STRIP_W, SAT_H);
        (picked, original, sat, opacity, hue)
    }
}

impl Workbench {
    /// Opens the color picker for the swatch at `pos` in group `g`'s editor.
    pub(super) fn open_color_picker(&mut self, g: usize, pos: Pos) {
        let Some(ed) = self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let Some((color, end)) = doc.swatches.on_line(pos.line).iter().find(|h| h.col == pos.col).and_then(|h| h.swatch) else { return };
        let text: String = doc.buffer.line(pos.line).chars().skip(pos.col).take(end.saturating_sub(pos.col)).collect();
        let original = Rgba::from_color(color);
        self.active_group = g;
        self.color_picker = Some(ColorPicker {
            g,
            doc: ed.doc,
            line: pos.line,
            start: pos.col,
            end,
            original,
            color: Hsva::from_rgba(original),
            written: original,
            presentation: guess_presentation(original, &text),
            version: doc.buffer.version(),
            rect: Rect::default(),
            saturation: None,
            opacity: None,
            hue: image(1, 360, |_, y| {
                let c = Hsva::new((y * 360.0).floor(), 1.0, 1.0, 1.0).to_rgba();
                [c.r, c.g, c.b, 255]
            }),
        });
    }

    pub(super) fn close_color_picker(&mut self) -> bool {
        self.color_picker.take().is_some()
    }

    /// Writes the picked color into the document.
    fn flush_color(&mut self) {
        let Some(p) = &self.color_picker else { return };
        let (rgba, label) = (p.rgba(), p.label());
        let (doc, line, start, end) = (p.doc, p.line, p.start, p.end);
        let current: String = self.docs[doc].as_ref().map(|d| d.buffer.line(line).chars().skip(start).take(end - start).collect()).unwrap_or_default();
        if current == label {
            return;
        }
        self.edit_doc(doc, vec![(Pos::new(line, start), Pos::new(line, end), label.clone())], false);
        let version = self.docs[doc].as_ref().map_or(0, |d| d.buffer.version());
        let p = self.color_picker.as_mut().unwrap();
        p.end = start + label.chars().count();
        p.written = rgba;
        p.version = version;
    }

    /// A press on the picker.
    pub(super) fn click_color_picker(&mut self, hit: Hit, x: f32, y: f32) {
        match hit {
            Hit::ColorPickerPicked => {
                if let Some(p) = &mut self.color_picker {
                    p.presentation = (p.presentation + 1) % 3;
                }
                self.flush_color();
            }
            Hit::ColorPickerOriginal => {
                if let Some(p) = &mut self.color_picker {
                    p.color = Hsva::from_rgba(p.original);
                }
                self.flush_color();
            }
            Hit::ColorPickerPart(part) => {
                self.drag = Some(Drag::ColorPicker(part));
                self.drag_color_picker(part, x, y);
            }
            _ => {}
        }
    }

    /// Moves what's being dragged to the pointer.
    pub(super) fn drag_color_picker(&mut self, part: Part, x: f32, y: f32) {
        let Some(p) = &mut self.color_picker else { return };
        let (_, _, sat, opacity, hue) = p.parts();
        let c = p.color;
        let frac = |r: Rect, v: f32| ((v - r.y) / r.h).clamp(0.0, 1.0) as f64;
        p.color = match part {
            Part::Saturation => {
                let s = ((x - sat.x) / sat.w).clamp(0.0, 1.0) as f64;
                Hsva::new(c.h, s, 1.0 - frac(sat, y), c.a)
            }
            Part::Opacity => Hsva::new(c.h, c.s, c.v, 1.0 - frac(opacity, y)),
            Part::Hue => {
                let h = (frac(hue, y) * 360.0).round();
                Hsva::new(if h == 360.0 { 0.0 } else { h }, c.s, c.v, c.a)
            }
        };
    }

    /// Letting go: the color goes into the document.
    pub(super) fn color_picker_released(&mut self) {
        self.flush_color();
    }

    /// Draws the picker over group `g`'s editor, next to its swatch; closes it when the color
    /// is gone (edited elsewhere, scrolled away, another editor).
    pub(super) fn draw_color_picker(&mut self, c: &mut Canvas, g: usize, editor: Rect) {
        let Some(p) = &self.color_picker else { return };
        if p.g != g {
            return;
        }
        let gr = &self.groups[g];
        let ed = gr.tabs.get(gr.active).filter(|e| e.doc == p.doc);
        let version = self.docs[p.doc].as_ref().map(|d| d.buffer.version());
        let swatch = ed.and_then(|e| e.swatch_hits.iter().find(|(_, pos)| *pos == Pos::new(p.line, p.start)).map(|(r, _)| *r));
        let (Some(swatch), true) = (swatch, version == Some(p.version)) else {
            self.color_picker = None;
            return;
        };
        // Above the color when there's room, like the standard hover; else below.
        let (w, h) = (WIDTH + 2.0, HEIGHT + 2.0);
        let y = if swatch.y - h - 4.0 >= editor.y { swatch.y - h - 4.0 } else { swatch.bottom() + 4.0 };
        let x = swatch.x.min(self.main_rect.right() - w - 4.0).max(4.0);
        let rect = Rect::new(x, y, w, h);
        let (bg, border, shadow) = (self.color("editorHoverWidget.background"), self.color("editorHoverWidget.border"), self.color("widget.shadow"));
        let p = self.color_picker.as_mut().unwrap();
        p.rect = rect;
        let (picked, original, sat, opacity, hue) = p.parts();
        let rgba = p.rgba();
        // Images for the current hue and color.
        if p.saturation.as_ref().is_none_or(|(h, _)| *h != p.color.h) {
            let base = Hsva::new(p.color.h, 1.0, 1.0, 1.0).to_rgba();
            let img = image(SAT_W as u32, SAT_H as u32, |x, y| {
                // The hue, then white fading out left to right, then black fading in downwards.
                let white = (1.0 - x).max(0.0);
                let mix = |ch: u8| {
                    let v = ch as f64 / 255.0 * (1.0 - white) + white;
                    ((v * (1.0 - y)) * 255.0).round() as u8
                };
                [mix(base.r), mix(base.g), mix(base.b), 255]
            });
            p.saturation = Some((p.color.h, img));
        }
        if p.opacity.as_ref().is_none_or(|(k, _)| *k != (rgba.r, rgba.g, rgba.b)) {
            let img = image(1, SAT_H as u32, |_, y| [rgba.r, rgba.g, rgba.b, ((1.0 - y) * 255.0).round() as u8]);
            p.opacity = Some(((rgba.r, rgba.g, rgba.b), img));
        }
        let (sat_img, opacity_img, hue_img) = (p.saturation.as_ref().unwrap().1.clone(), p.opacity.as_ref().unwrap().1.clone(), p.hue.clone());
        let (color, original_color, label) = (p.color, p.original, p.label());

        c.push_layer();
        c.shadow(rect, 3.0, shadow);
        c.bordered(rect, bg, border, 1.0, 0.0);
        // Header: the picked color with its text, and the original color.
        let header = Rect::new(picked.x, picked.y, WIDTH, HEADER_H);
        checkerboard(c, header);
        c.fill(picked, rgba.color());
        c.fill(original, original_color.color());
        let light_text = if rgba.a < 0.5 { Rgba::from_color(bg).is_lighter() } else { rgba.is_lighter() };
        let fg = if light_text { theme::Color::rgba8(0, 0, 0, 255) } else { theme::Color::rgba8(255, 255, 255, 255) };
        let style = TextStyle::ui(13.0, fg);
        let tw = c.measure(&label, &style);
        let x0 = picked.x + ((picked.w - tw - 19.0) / 2.0).max(4.0);
        c.icon(&crate::icons::COLOR_MODE, x0, picked.y + 5.0, 14.0, fg);
        c.text_in(Rect::new(x0 + 19.0, picked.y, picked.right() - x0 - 19.0, HEADER_H), &label, &style);
        // The gradients, then (in a layer above, since images draw over their layer's quads)
        // the selection circle and the sliders.
        c.image(sat, &sat_img);
        checkerboard(c, opacity);
        c.image(opacity, &opacity_img);
        c.image(hue, &hue_img);
        c.push_layer();
        let (sx, sy) = (sat.x + color.s as f32 * sat.w, sat.y + (1.0 - color.v as f32) * sat.h);
        c.push_clip(sat);
        let white = theme::Color::rgba8(255, 255, 255, 255);
        // `box-shadow: 0 0 2px rgba(0, 0, 0, 0.8)`, as a thin dark ring.
        c.bordered(Rect::new(sx - 6.0, sy - 6.0, 13.0, 13.0), theme::Color::TRANSPARENT, theme::Color::rgba8(0, 0, 0, 150), 1.5, 6.5);
        c.bordered(Rect::new(sx - 5.0, sy - 5.0, 11.0, 11.0), theme::Color::TRANSPARENT, white, 1.0, 5.5);
        c.pop_clip();
        let slider = |c: &mut Canvas, r: Rect, value: f64| {
            let y = r.y + (1.0 - value as f32) * (r.h - 4.0);
            let s = Rect::new(r.x - 2.0, y, r.w + 4.0, 4.0);
            c.bordered(s.inset(-1.0, -1.0), theme::Color::TRANSPARENT, theme::Color::rgba8(0, 0, 0, 160), 1.0, 0.0);
            c.bordered(s, theme::Color::TRANSPARENT, theme::Color::rgba8(255, 255, 255, 181), 1.0, 0.0);
        };
        slider(c, opacity, color.a);
        slider(c, hue, 1.0 - color.h / 360.0);
        self.hits.push((rect, Hit::ColorPickerBox));
        self.hits.push((picked, Hit::ColorPickerPicked));
        self.hits.push((original, Hit::ColorPickerOriginal));
        self.hits.push((sat, Hit::ColorPickerPart(Part::Saturation)));
        self.hits.push((opacity.inset(-2.0, 0.0), Hit::ColorPickerPart(Part::Opacity)));
        self.hits.push((hue.inset(-2.0, 0.0), Hit::ColorPickerPart(Part::Hue)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(r: u8, g: u8, b: u8, a: f64) -> Rgba {
        Rgba { r, g, b, a }
    }

    #[test]
    fn formats_colors() {
        assert_eq!(presentations(rgba(255, 0, 0, 1.0)), ["rgb(255, 0, 0)", "hsl(0, 100%, 50%)", "#ff0000"]);
        assert_eq!(presentations(rgba(0, 128, 255, 0.5)), ["rgba(0, 128, 255, 0.5)", "hsla(210, 100%, 50%, 0.50)", "#0080ff80"]);
        assert_eq!(presentations(rgba(51, 51, 51, 0.25)), ["rgba(51, 51, 51, 0.25)", "hsla(0, 0%, 20%, 0.25)", "#33333340"]);
    }

    #[test]
    fn guesses_the_texts_format() {
        let red = rgba(255, 0, 0, 1.0);
        assert_eq!(guess_presentation(red, "#FF0000"), 2);
        assert_eq!(guess_presentation(red, "hsl(0, 100%, 50%)"), 1);
        assert_eq!(guess_presentation(red, "rgb(255,0,0)"), 0);
        // A short hex matches no presentation, so the first one is kept.
        assert_eq!(guess_presentation(red, "#f00"), 0);
    }

    #[test]
    fn hsv_round_trips() {
        for c in [rgba(255, 0, 0, 1.0), rgba(0, 128, 255, 0.5), rgba(18, 52, 86, 1.0), rgba(255, 255, 255, 1.0), rgba(0, 0, 0, 0.0)] {
            let back = Hsva::from_rgba(c).to_rgba();
            let close = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 1;
            assert!(close(back.r, c.r) && close(back.g, c.g) && close(back.b, c.b) && back.a == c.a, "{c:?} -> {back:?}");
        }
    }
}
