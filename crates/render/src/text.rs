//! Text shaping (cosmic-text) with a per-line cache, plus glyph and icon rasterization.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, LayoutGlyph, Metrics, Shaping, SwashCache, SwashContent,
    Weight,
};
use theme::Color;

use crate::atlas::{Atlas, Slot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    /// The platform UI font (SF Pro on macOS), used for the workbench chrome.
    Ui,
    /// The editor font (Menlo on macOS).
    Mono,
}

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub font: Font,
    pub size: f32,
    pub line_height: f32,
    pub weight: u16,
    pub italic: bool,
    pub color: Color,
}

impl TextStyle {
    pub fn ui(size: f32, color: Color) -> Self {
        Self { font: Font::Ui, size, line_height: (size * 1.4).round(), weight: 400, italic: false, color }
    }

    pub fn mono(size: f32, line_height: f32, color: Color) -> Self {
        Self { font: Font::Mono, size, line_height, weight: 400, italic: false, color }
    }

    pub fn weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }

    pub fn italic(mut self, italic: bool) -> Self {
        self.italic = italic;
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    fn hash_into(&self, h: &mut impl Hasher) {
        self.font.hash(h);
        self.size.to_bits().hash(h);
        self.line_height.to_bits().hash(h);
        self.weight.hash(h);
        self.italic.hash(h);
    }
}

/// How many cells of a monospace grid `c` takes: 2 for wide (East Asian) characters, 0 for
/// combining marks, else 1. The editor's columns use the same rule.
pub fn char_cells(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(1)
}

/// Puts monospace glyphs on the grid of `cell`-wide cells: a glyph from a fallback font (CJK,
/// emoji) has its own advance, so it's centered in the cells its text takes instead, and the
/// text after it stays in its columns. Right-to-left runs are left as shaped.
fn snap_to_cells(text: &str, line: &mut ShapedLine, cell: f32) {
    if cell <= 0.0 || line.glyphs.iter().any(|g| g.level.is_rtl()) {
        return;
    }
    let mut x = 0.0;
    // The last cluster placed: (start byte, its shaped x, its new x).
    let mut last: Option<(usize, f32, f32)> = None;
    for g in &mut line.glyphs {
        let cells: usize = text.get(g.start..g.end).map_or(1, |t| t.chars().map(char_cells).sum());
        match last {
            // More glyphs of the same cluster, or a zero-width one: kept where they were
            // relative to the glyph before.
            Some((start, shaped, placed)) if start == g.start || cells == 0 => {
                g.x = placed + (g.x - shaped);
            }
            _ => {
                let slot = cells as f32 * cell;
                let shaped = g.x;
                g.x = x + ((slot - g.w) / 2.0).max(0.0);
                last = Some((g.start, shaped, g.x));
                x += slot;
            }
        }
    }
    line.width = x;
}

pub(crate) struct ShapedLine {
    pub glyphs: Vec<LayoutGlyph>,
    pub width: f32,
    pub baseline: f32,
    last_used: u64,
}

/// A vector icon: SVG path data drawn in a `viewbox`×`viewbox` coordinate space.
#[derive(Clone, Copy, Debug)]
pub struct Icon {
    pub path: &'static str,
    pub viewbox: f32,
    /// `Some(width)` strokes the path (in viewbox units); `None` fills it.
    pub stroke: Option<f32>,
}

/// Rotation steps per full turn for spinning icons.
pub const ICON_TURNS: u32 = 24;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum MaskKey {
    Glyph(CacheKey),
    Icon { path: usize, size: u32, stroke: u32, turn: u32 },
}

pub struct TextSystem {
    pub(crate) fonts: FontSystem,
    swash: SwashCache,
    ui_family: String,
    /// Whether the interface uses the editor's font (`mono_family`) instead of `ui_family`.
    ui_is_mono: bool,
    mono_family: String,
    /// The `editor.fontFamily`-style list `mono_family` was chosen from.
    mono_request: String,
    lines: HashMap<u64, ShapedLine>,
    glyphs: HashMap<CacheKey, Option<(Slot, [i32; 2], bool)>>,
    pub(crate) mask_atlas: Atlas<MaskKey>,
    pub(crate) color_atlas: Atlas<CacheKey>,
    frame: u64,
}

/// The family name of the SF Mono that comes with macOS.
const SYSTEM_MONO: &str = ".SF NS Mono";

impl TextSystem {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let fonts = FontSystem::new();
        let has = |name: &str| fonts.db().faces().any(|f| f.families.iter().any(|(n, _)| n == name));
        let ui_family = ["System Font", "Helvetica Neue", "Helvetica"]
            .into_iter()
            .find(|f| has(f))
            .unwrap_or("Helvetica")
            .to_string();
        // Until the editor's setting arrives: SF Mono (macOS's own copy if not installed by name).
        let mono_family = ["SF Mono", SYSTEM_MONO, "Menlo", "Monaco", "Courier"]
            .into_iter()
            .find(|f| has(f))
            .unwrap_or("Menlo")
            .to_string();
        Self {
            fonts,
            swash: SwashCache::new(),
            ui_family,
            ui_is_mono: true,
            mono_family,
            mono_request: String::new(),
            lines: HashMap::new(),
            glyphs: HashMap::new(),
            mask_atlas: Atlas::new(device, queue, wgpu::TextureFormat::R8Unorm, "mask atlas"),
            color_atlas: Atlas::new(device, queue, wgpu::TextureFormat::Rgba8Unorm, "color atlas"),
            frame: 0,
        }
    }

    /// The interface font: the editor's (`mono`) or the system's.
    pub(crate) fn set_ui_mono(&mut self, mono: bool) {
        if mono != self.ui_is_mono {
            self.ui_is_mono = mono;
            self.lines.clear();
        }
    }

    /// Picks the monospace font from a CSS-style family list ("Menlo, 'Courier New',
    /// monospace"): the first installed family wins; generic `monospace` means Menlo. Returns
    /// the first family when it isn't installed (only when the list changed).
    pub(crate) fn set_mono_families(&mut self, list: &str) -> Vec<String> {
        if list == self.mono_request {
            return Vec::new();
        }
        self.mono_request = list.to_string();
        let db = self.fonts.db();
        let installed = |name: &str| db.faces().any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)));
        let names: Vec<&str> = list
            .split(',')
            .map(|f| f.trim().trim_matches(|c| c == '\'' || c == '"').trim())
            .filter(|f| !f.is_empty())
            .map(|f| if f.eq_ignore_ascii_case("monospace") { "Menlo" } else { f })
            // macOS ships SF Mono as a hidden system font; use it unless SF Mono itself is installed.
            .map(|f| if f.eq_ignore_ascii_case("SF Mono") && !installed(f) { SYSTEM_MONO } else { f })
            .collect();
        // Later families are fallbacks; only the first one not being there is worth saying.
        let missing = names.first().filter(|f| !installed(f)).map(|f| f.to_string()).into_iter().collect();
        let chosen = names
            .into_iter()
            .find(|f| installed(f))
            .map(|f| {
                // Use the database's spelling of the name.
                db.faces()
                    .flat_map(|face| face.families.iter())
                    .find(|(n, _)| n.eq_ignore_ascii_case(f))
                    .map_or(f.to_string(), |(n, _)| n.clone())
            })
            .unwrap_or_else(|| "Menlo".to_string());
        if chosen != self.mono_family {
            self.mono_family = chosen;
            self.lines.clear();
        }
        missing
    }

    pub(crate) fn end_frame(&mut self) {
        self.frame += 1;
        if self.frame % 120 == 0 {
            let frame = self.frame;
            self.lines.retain(|_, l| frame - l.last_used < 240);
        }
    }

    /// Shapes a single line. `spans` optionally colors byte ranges of `text`.
    pub(crate) fn shape(&mut self, text: &str, spans: &[(usize, usize, Color)], style: &TextStyle) -> &ShapedLine {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        style.hash_into(&mut h);
        if !spans.is_empty() {
            let c = style.color;
            (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()).hash(&mut h);
        }
        for (a, b, c) in spans {
            (a, b, c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()).hash(&mut h);
        }
        let key = h.finish();
        let frame = self.frame;
        if !self.lines.contains_key(&key) {
            let line = self.shape_uncached(text, spans, style);
            self.lines.insert(key, line);
        }
        let line = self.lines.get_mut(&key).unwrap();
        line.last_used = frame;
        line
    }

    fn shape_uncached(&mut self, text: &str, spans: &[(usize, usize, Color)], style: &TextStyle) -> ShapedLine {
        let family = match style.font {
            Font::Ui if self.ui_is_mono => self.mono_family.as_str(),
            Font::Ui => self.ui_family.as_str(),
            Font::Mono => self.mono_family.as_str(),
        };
        let attrs = Attrs::new().family(Family::Name(family)).weight(Weight(style.weight));
        let attrs = if style.italic { attrs.style(cosmic_text::Style::Italic) } else { attrs };
        let mut buffer = Buffer::new_empty(Metrics::new(style.size, style.line_height));
        buffer.set_size(None, None);
        if spans.is_empty() {
            buffer.set_text(text, &attrs, Shaping::Advanced, None);
        } else {
            // Spans may leave gaps (plain text between tokens); fill them with the default color.
            let mut runs = Vec::with_capacity(spans.len() * 2 + 1);
            let mut pos = 0;
            for &(a, b, c) in spans {
                if a < pos || b > text.len() || a >= b {
                    continue;
                }
                if a > pos {
                    runs.push((pos, a, style.color));
                }
                runs.push((a, b, c));
                pos = b;
            }
            if pos < text.len() {
                runs.push((pos, text.len(), style.color));
            }
            let rich = runs.into_iter().map(|(a, b, c)| {
                let color = cosmic_text::Color::rgba(
                    (c.r * 255.0) as u8,
                    (c.g * 255.0) as u8,
                    (c.b * 255.0) as u8,
                    (c.a * 255.0) as u8,
                );
                (&text[a..b], attrs.clone().color(color))
            });
            buffer.set_rich_text(rich, &attrs, Shaping::Advanced, None);
        }
        buffer.shape_until_scroll(&mut self.fonts, false);
        let mut line = ShapedLine { glyphs: Vec::new(), width: 0.0, baseline: style.line_height * 0.8, last_used: 0 };
        if let Some(run) = buffer.layout_runs().next() {
            line.glyphs = run.glyphs.to_vec();
            line.width = run.line_w;
            line.baseline = run.line_y;
        }
        if matches!(style.font, Font::Mono) && !text.is_ascii() {
            let cell = self.shape_uncached("0000000000", &[], style).width / 10.0;
            snap_to_cells(text, &mut line, cell);
        }
        line
    }

    /// Rasterizes (or fetches) a glyph. Returns the slot, bearing and whether it is a color glyph.
    pub(crate) fn glyph(&mut self, key: CacheKey) -> Option<(Slot, [i32; 2], bool)> {
        if let Some(entry) = self.glyphs.get(&key) {
            return *entry;
        }
        let entry = self.swash.get_image_uncached(&mut self.fonts, key).and_then(|image| {
            let p = image.placement;
            let data = image.data;
            match image.content {
                SwashContent::Mask => self
                    .mask_atlas
                    .get_or_insert(&MaskKey::Glyph(key), || Some((p.width, p.height, p.left, p.top, data)))
                    .map(|(s, b)| (s, b, false)),
                SwashContent::Color => self
                    .color_atlas
                    .get_or_insert(&key, || Some((p.width, p.height, p.left, p.top, data)))
                    .map(|(s, b)| (s, b, true)),
                SwashContent::SubpixelMask => None,
            }
        });
        if self.mask_atlas.overflowed || self.color_atlas.overflowed {
            // An atlas was reset, so every cached slot is stale.
            self.glyphs.clear();
        }
        self.glyphs.insert(key, entry);
        entry
    }

    /// Rasterizes `icon` at `size_px`, rotated by `turn` of `ICON_TURNS` steps (for spinners).
    pub(crate) fn icon(&mut self, icon: &Icon, size_px: u32, turn: u32) -> Option<Slot> {
        let turn = turn % ICON_TURNS;
        let key = MaskKey::Icon {
            path: icon.path.as_ptr() as usize,
            size: size_px,
            stroke: icon.stroke.map_or(0, f32::to_bits),
            turn,
        };
        self.mask_atlas
            .get_or_insert(&key, || {
                use zeno::{Mask, Stroke, Transform};
                let s = size_px as f32 / icon.viewbox;
                let c = icon.viewbox / 2.0;
                let angle = zeno::Angle::from_degrees(turn as f32 * 360.0 / ICON_TURNS as f32);
                let transform = Transform::rotation_about((c, c), angle).then_scale(s, s);
                let mut mask = Mask::new(icon.path);
                mask.transform(Some(transform)).size(size_px, size_px);
                if let Some(width) = icon.stroke {
                    let mut stroke = Stroke::new(width);
                    stroke.cap(zeno::Cap::Round).join(zeno::Join::Round);
                    mask.style(stroke);
                }
                let (data, placement) = mask.render();
                Some((placement.width, placement.height, 0, 0, data))
            })
            .map(|(slot, _)| slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wide chars from fallback fonts sit centered in two cells of the monospace grid, and the
    /// text after them stays in its columns.
    #[test]
    fn wide_chars_take_two_cells() {
        let Ok(mut r) = crate::Renderer::offscreen((8, 8), 1.0) else { return }; // no GPU here
        let t = &mut r.text;
        let style = TextStyle::mono(13.0, 18.0, Color::rgba8(255, 255, 255, 255));
        let cell = t.shape("0000000000", &[], &style).width / 10.0;
        let line = t.shape("a日b", &[], &style);
        let xs: Vec<(usize, f32, f32)> = line.glyphs.iter().map(|g| (g.start, g.x, g.w)).collect();
        assert!((line.width - 4.0 * cell).abs() < 0.01, "{} vs {cell}", line.width);
        let wide = xs.iter().find(|g| g.0 == 1).unwrap();
        assert!(wide.1 >= cell - 0.01 && wide.1 + wide.2 <= 3.0 * cell + 0.01, "{xs:?}");
        let b = xs.iter().find(|g| g.0 == 4).unwrap();
        assert!((b.1 - 3.0 * cell).abs() < 0.01, "{xs:?}");
        assert_eq!(char_cells('a'), 1);
        assert_eq!(char_cells('日'), 2);
        assert_eq!(char_cells('\u{301}'), 0); // a combining accent
    }

    /// "SF Mono" finds the copy macOS ships (a hidden system font) when it isn't installed under
    /// its own name; a missing first choice is reported, missing fallbacks aren't.
    #[test]
    fn picks_mono_fonts() {
        let Ok(mut r) = crate::Renderer::offscreen((8, 8), 1.0) else { return }; // no GPU here
        let t = &mut r.text;
        if !t.fonts.db().faces().any(|f| f.families.iter().any(|(n, _)| n == SYSTEM_MONO || n == "SF Mono")) {
            return; // not macOS
        }
        assert!(t.set_mono_families("SF Mono, Menlo").is_empty());
        assert!(t.mono_family == SYSTEM_MONO || t.mono_family == "SF Mono", "{}", t.mono_family);
        // The default list: SF Mono first, the rest only fallbacks.
        assert!(t.set_mono_families("SF Mono, Menlo, Monaco, 'Courier New', monospace").is_empty());
        assert!(t.mono_family == SYSTEM_MONO || t.mono_family == "SF Mono", "{}", t.mono_family);
        assert!(t.set_mono_families("Menlo, Monaco").is_empty());
        assert_eq!(t.mono_family, "Menlo");
        assert_eq!(t.set_mono_families("No Such Font, Menlo"), vec!["No Such Font".to_string()]);
        assert_eq!(t.mono_family, "Menlo");
    }
}
