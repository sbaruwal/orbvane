//! Finds color values in text for the inline color swatches, like the default document
//! color provider (`defaultDocumentColorsComputer.ts`): `#rgb`, `#rgba`, `#rrggbb` and
//! `#rrggbbaa` at the start of a line or after a quote or whitespace, and `rgb()`, `rgba()`,
//! `hsl()`, `hsla()` with valid parameters (commas, or CSS Level 4 spaces and `/ alpha`).

use std::sync::OnceLock;

use regex::Regex;
use theme::Color;

/// A color found in a line: its char columns and value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    pub start: usize,
    pub end: usize,
    pub color: Color,
}

const BYTE: &str = r"(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9][0-9]|[0-9])";
const HUE: &str = r"((?:360(?:\.0+)?|(?:36[0]|3[0-5][0-9]|[12][0-9][0-9]|[1-9]?[0-9])(?:\.\d+)?))";
const PERCENT: &str = r"(100(?:\.0+)?|\d{1,2}[.]\d*|\d{1,2})%";

struct Patterns {
    function: Regex,
    rgb: Regex,
    rgba: Regex,
    hsl: Regex,
    hsla: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: String| Regex::new(&s).unwrap();
        Patterns {
            function: re(r"\b(rgb|rgba|hsl|hsla)(\([0-9\s,.%/]*\))".into()),
            rgb: re(format!(r"^\(\s*{BYTE}\s*[\s,]\s*{BYTE}\s*[\s,]\s*{BYTE}\s*\)$")),
            rgba: re(format!(r"^\(\s*{BYTE}\s*[\s,]\s*{BYTE}\s*[\s,]\s*{BYTE}\s*(?:[\s,]|[\s]*/)\s*(0[.][0-9]+|[.][0-9]+|[01][.]|[01])\s*\)$")),
            hsl: re(format!(r"^\(\s*{HUE}\s*[\s,]\s*{PERCENT}\s*[\s,]\s*{PERCENT}\s*\)$")),
            hsla: re(format!(r"^\(\s*{HUE}\s*[\s,]\s*{PERCENT}\s*[\s,]\s*{PERCENT}\s*(?:[\s,]|[\s]*/)\s*(0[.][0-9]+|[.][0-9]+|[01][.]0*|[01])\s*\)$")),
        }
    })
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// HSL (hue in degrees, saturation and lightness 0..1) to RGB 0..255, rounded.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let h = (h % 360.0) / 360.0;
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let channel = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u8
    };
    (channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0))
}

fn with_alpha(r: u8, g: u8, b: u8, a: f64) -> Color {
    Color { a: a as f32, ..Color::rgba8(r, g, b, 255) }
}

/// The color of `rgb(...)`-style `scheme` with parameters `params` ("(1, 2, 3)"), if valid.
fn function_color(scheme: &str, params: &str) -> Option<Color> {
    let p = patterns();
    let (re, hsl) = match scheme {
        "rgb" => (&p.rgb, false),
        "rgba" => (&p.rgba, false),
        "hsl" => (&p.hsl, true),
        _ => (&p.hsla, true),
    };
    let caps = re.captures(params)?;
    let n = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<f64>().ok());
    let alpha = if scheme.ends_with('a') { n(4)? } else { 1.0 };
    Some(if hsl {
        let (r, g, b) = hsl_to_rgb(n(1)?, n(2)? / 100.0, n(3)? / 100.0);
        with_alpha(r, g, b, alpha)
    } else {
        with_alpha(n(1)? as u8, n(2)? as u8, n(3)? as u8, alpha)
    })
}

/// The colors in one line of text.
pub fn find_in_line(line: &str) -> Vec<Found> {
    let col = |byte: usize| line[..byte].chars().count();
    let mut out = Vec::new();
    for caps in patterns().function.captures_iter(line) {
        let (all, scheme, params) = (caps.get(0).unwrap(), &caps[1], &caps[2]);
        if let Some(color) = function_color(scheme, params) {
            out.push(Found { start: col(all.start()), end: col(all.end()), color });
        }
    }
    // Hex colors: at the start of the line or after a quote or whitespace, ending at a word
    // boundary.
    let bytes = line.as_bytes();
    for (i, _) in line.match_indices('#') {
        if i > 0 && !matches!(bytes[i - 1], b'\'' | b'"') && !(bytes[i - 1] as char).is_whitespace() {
            continue;
        }
        let digits = line[i + 1..].chars().take_while(char::is_ascii_hexdigit).count();
        let next = line[i + 1 + digits..].chars().next();
        if !matches!(digits, 3 | 4 | 6 | 8) || next.is_some_and(is_word) {
            continue;
        }
        if let Some(color) = Color::hex(&line[i..i + 1 + digits]) {
            out.push(Found { start: col(i), end: col(i + 1 + digits), color });
        }
    }
    out.sort_by_key(|f| f.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(f: &Found) -> (u8, u8, u8, f32) {
        let c = f.color;
        ((c.r * 255.0).round() as u8, (c.g * 255.0).round() as u8, (c.b * 255.0).round() as u8, c.a)
    }

    #[test]
    fn finds_hex_colors() {
        let found = find_in_line(r##"#fff a: "#00ff0080", b: '#1234' c:#abcdef #12345 x#123 #1234567g"##);
        let cols: Vec<(usize, usize)> = found.iter().map(|f| (f.start, f.end)).collect();
        assert_eq!(cols, [(0, 4), (9, 18), (25, 30)]);
        assert_eq!(rgba(&found[1]), (0, 255, 0, 128.0 / 255.0));
        assert_eq!(rgba(&found[2]), (0x11, 0x22, 0x33, 0x44 as f32 / 255.0));
    }

    #[test]
    fn finds_color_functions() {
        let line = "a: rgb(255, 0, 0); b: rgba(0 0 255 / .5); c: hsl(120, 100%, 50%); d: hsla(240 100% 50% / 0.25); e: rgb(256, 0, 0); frgb(1,2,3)";
        let found = find_in_line(line);
        let got: Vec<_> = found.iter().map(|f| (&line[f.start..f.end], rgba(f))).collect();
        assert_eq!(
            got,
            [
                ("rgb(255, 0, 0)", (255, 0, 0, 1.0)),
                ("rgba(0 0 255 / .5)", (0, 0, 255, 0.5)),
                ("hsl(120, 100%, 50%)", (0, 255, 0, 1.0)),
                ("hsla(240 100% 50% / 0.25)", (0, 0, 255, 0.25)),
            ]
        );
    }

    #[test]
    fn columns_are_chars() {
        let found = find_in_line("é '#fff'");
        assert_eq!((found[0].start, found[0].end), (3, 7));
    }
}
