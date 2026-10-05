//! Default-color expressions from the color registry, and the color math to evaluate
//! them exactly like `Color` class (HSLA lighten/darken, alpha scaling...).

use crate::Color;

/// A registry default: a color, a reference to another color id, or a transform of those.
/// `registry.rs` (generated) holds one per theme kind for every color id.
#[derive(Debug)]
pub enum Expr {
    /// No default: the color is unset unless the theme defines it.
    Null,
    /// 0xRRGGBBAA.
    Hex(u32),
    Ref(&'static str),
    Darken(&'static Expr, f32),
    Lighten(&'static Expr, f32),
    Transparent(&'static Expr, f32),
    /// Blend a translucent color onto an opaque background.
    Opaque(&'static Expr, &'static Expr),
    /// The first value that resolves.
    OneOf(&'static [Expr]),
    /// `then` if the theme itself defines the color id, otherwise `else`.
    IfDefined(&'static str, &'static Expr, &'static Expr),
    LessProminent(&'static Expr, &'static Expr, f32, f32),
}

/// RGBA: integer channels, alpha rounded to 3 decimals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: f32,
}

fn round3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

impl Rgba {
    fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        // JS `| 0` truncates.
        let ch = |v: f32| v.clamp(0.0, 255.0) as u8;
        Self { r: ch(r), g: ch(g), b: ch(b), a: round3(a.clamp(0.0, 1.0)) }
    }

    pub(crate) fn from_hex(v: u32) -> Self {
        let [r, g, b, a] = v.to_be_bytes();
        Self { r, g, b, a: round3(a as f32 / 255.0) }
    }

    pub(crate) fn from_color(c: Color) -> Self {
        let ch = |v: f32| (v * 255.0).round() as u8;
        Self { r: ch(c.r), g: ch(c.g), b: ch(c.b), a: round3(c.a) }
    }

    pub(crate) fn to_color(self) -> Color {
        Color::rgba8(self.r, self.g, self.b, (self.a * 255.0).round() as u8)
    }

    fn to_hsla(self) -> (f32, f32, f32, f32) {
        let (r, g, b) = (self.r as f32 / 255.0, self.g as f32 / 255.0, self.b as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (min + max) / 2.0;
        let chroma = max - min;
        let (mut h, mut s) = (0.0, 0.0);
        if chroma > 0.0 {
            s = (if l <= 0.5 { chroma / (2.0 * l) } else { chroma / (2.0 - 2.0 * l) }).min(1.0);
            h = if max == r {
                (g - b) / chroma + if g < b { 6.0 } else { 0.0 }
            } else if max == g {
                (b - r) / chroma + 2.0
            } else {
                (r - g) / chroma + 4.0
            };
            h = (h * 60.0).round();
        }
        // HSLA's constructor clamps and rounds like this.
        (h.clamp(0.0, 360.0).trunc(), round3(s.clamp(0.0, 1.0)), round3(l.clamp(0.0, 1.0)), round3(self.a))
    }

    fn from_hsla(h: f32, s: f32, l: f32, a: f32) -> Self {
        let (h, s, l) = (h.clamp(0.0, 360.0).trunc(), round3(s.clamp(0.0, 1.0)), round3(l.clamp(0.0, 1.0)));
        let (r, g, b) = if s == 0.0 {
            (l, l, l)
        } else {
            let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
            let p = 2.0 * l - q;
            let hk = h / 360.0;
            (hue2rgb(p, q, hk + 1.0 / 3.0), hue2rgb(p, q, hk), hue2rgb(p, q, hk - 1.0 / 3.0))
        };
        Self::new((r * 255.0).round(), (g * 255.0).round(), (b * 255.0).round(), a)
    }

    fn lighten(self, f: f32) -> Self {
        let (h, s, l, a) = self.to_hsla();
        Self::from_hsla(h, s, l + l * f, a)
    }

    fn darken(self, f: f32) -> Self {
        let (h, s, l, a) = self.to_hsla();
        Self::from_hsla(h, s, l - l * f, a)
    }

    fn transparent(self, f: f32) -> Self {
        Self::new(self.r as f32, self.g as f32, self.b as f32, self.a * f)
    }

    fn make_opaque(self, bg: Self) -> Self {
        if self.a == 1.0 || bg.a != 1.0 {
            return self;
        }
        let mix = |b: u8, c: u8| b as f32 - self.a * (b as f32 - c as f32);
        Self::new(mix(bg.r, self.r), mix(bg.g, self.g), mix(bg.b, self.b), 1.0)
    }

    fn luminance(self) -> f32 {
        let comp = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let l = 0.2126 * comp(self.r) + 0.7152 * comp(self.g) + 0.0722 * comp(self.b);
        (l * 10000.0).round() / 10000.0
    }

    fn lighter_than(self, bg: Self, factor: f32) -> Self {
        let (l1, l2) = (self.luminance(), bg.luminance());
        if l1 > l2 {
            return self;
        }
        let factor = if factor == 0.0 { 0.5 } else { factor };
        self.lighten(factor * (l2 - l1) / l2)
    }

    fn darker_than(self, bg: Self, factor: f32) -> Self {
        let (l1, l2) = (self.luminance(), bg.luminance());
        if l1 < l2 {
            return self;
        }
        let factor = if factor == 0.0 { 0.5 } else { factor };
        self.darken(factor * (l1 - l2) / l1)
    }
}

fn hue2rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

/// Evaluates `e`, looking up referenced ids with `get` (which applies theme values first).
pub(crate) fn eval(e: &Expr, get: &mut dyn FnMut(&str) -> Option<Rgba>, defines: &dyn Fn(&str) -> bool) -> Option<Rgba> {
    Some(match e {
        Expr::Null => return None,
        Expr::Hex(v) => Rgba::from_hex(*v),
        Expr::Ref(id) => return get(id),
        Expr::Darken(v, f) => eval(v, get, defines)?.darken(*f),
        Expr::Lighten(v, f) => eval(v, get, defines)?.lighten(*f),
        Expr::Transparent(v, f) => eval(v, get, defines)?.transparent(*f),
        Expr::Opaque(v, bg) => {
            let c = eval(v, get, defines)?;
            match eval(bg, get, defines) {
                Some(bg) => c.make_opaque(bg),
                None => c,
            }
        }
        Expr::OneOf(values) => return values.iter().find_map(|v| eval(v, get, defines)),
        Expr::IfDefined(id, then, els) => return eval(if defines(id) { then } else { els }, get, defines),
        Expr::LessProminent(v, bg, factor, transparency) => {
            let from = eval(v, get, defines)?;
            match eval(bg, get, defines) {
                None => from.transparent(factor * transparency),
                Some(bg) if from.luminance() < bg.luminance() => from.lighter_than(bg, *factor).transparent(*transparency),
                Some(bg) => from.darker_than(bg, *factor).transparent(*transparency),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_color_math() {
        // Values checked against Color class.
        let c = Rgba::from_hex(0x0078D4FF);
        assert_eq!(c.transparent(0.5), Rgba { r: 0, g: 0x78, b: 0xD4, a: 0.5 });
        let (h, s, l, _) = c.to_hsla();
        assert_eq!((h, s, l), (206.0, 1.0, 0.416));
        assert_eq!(Rgba::from_hsla(h, s, l, 1.0), Rgba { r: 0, g: 120, b: 212, a: 1.0 });
        // Lightening white stays white; darkening scales lightness.
        assert_eq!(Rgba::from_hex(0xFFFFFFFF).lighten(0.2), Rgba::from_hex(0xFFFFFFFF));
        assert_eq!(Rgba::from_hex(0x808080FF).darken(0.5), Rgba { r: 64, g: 64, b: 64, a: 1.0 });
        let opaque = Rgba::from_hex(0xFFFFFF80).make_opaque(Rgba::from_hex(0x000000FF));
        assert_eq!(opaque, Rgba { r: 128, g: 128, b: 128, a: 1.0 });
    }
}
