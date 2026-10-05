//! CSS colors: the named colors, and reading colors out of values (for swatches).

/// CSS's named colors as 0xRRGGBB.
pub const NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

/// An RGBA color, each channel 0..1.
pub type Rgba = [f32; 4];

pub fn named(name: &str) -> Option<Rgba> {
    if name.eq_ignore_ascii_case("transparent") {
        return Some([0.0, 0.0, 0.0, 0.0]);
    }
    let &(_, rgb) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
    Some([((rgb >> 16) & 0xff) as f32 / 255.0, ((rgb >> 8) & 0xff) as f32 / 255.0, (rgb & 0xff) as f32 / 255.0, 1.0])
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
pub fn hex(text: &str) -> Option<Rgba> {
    let h = text.strip_prefix('#')?;
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| (v * 17) as f32 / 255.0);
    let pair = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some(match h.len() {
        3 => [digit(0)?, digit(1)?, digit(2)?, 1.0],
        4 => [digit(0)?, digit(1)?, digit(2)?, digit(3)?],
        6 => [pair(0)?, pair(2)?, pair(4)?, 1.0],
        8 => [pair(0)?, pair(2)?, pair(4)?, pair(6)?],
        _ => return None,
    })
}

/// `rgb()`/`rgba()`/`hsl()`/`hsla()`/`hwb()` with numeric arguments (comma or space
/// separated, an optional `/ alpha`).
pub fn function(name: &str, args: &str) -> Option<Rgba> {
    let parts: Vec<&str> = args.split(|c: char| c == ',' || c == '/' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
    if parts.len() < 3 || parts.len() > 4 {
        return None;
    }
    let number = |s: &str| -> Option<f32> { s.trim_end_matches('%').trim_end_matches("deg").parse::<f32>().ok() };
    let alpha = match parts.get(3) {
        Some(a) if a.ends_with('%') => number(a)? / 100.0,
        Some(a) => number(a)?,
        None => 1.0,
    };
    let channel = |s: &str| -> Option<f32> { Some(if s.ends_with('%') { number(s)? / 100.0 } else { number(s)? / 255.0 }) };
    let percent = |s: &str| -> Option<f32> { Some(number(s)? / 100.0) };
    let rgb = match name.to_ascii_lowercase().as_str() {
        "rgb" | "rgba" => [channel(parts[0])?, channel(parts[1])?, channel(parts[2])?],
        "hsl" | "hsla" => hsl_to_rgb(number(parts[0])?, percent(parts[1])?, percent(parts[2])?),
        "hwb" => {
            let (w, b) = (percent(parts[1])?, percent(parts[2])?);
            let [r, g, bl] = hsl_to_rgb(number(parts[0])?, 1.0, 0.5);
            let scale = |c: f32| if w + b >= 1.0 { w / (w + b) } else { c * (1.0 - w - b) + w };
            [scale(r), scale(g), scale(bl)]
        }
        _ => return None,
    };
    let clamp = |v: f32| v.clamp(0.0, 1.0);
    Some([clamp(rgb[0]), clamp(rgb[1]), clamp(rgb[2]), clamp(alpha)])
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r + m, g + m, b + m]
}

/// Functions that make colors, as completion snippets.
pub const FUNCTIONS: &[(&str, &str, &str)] = &[
    ("rgb", "rgb(${1:red}, ${2:green}, ${3:blue})", "Creates a Color from red, green, and blue values."),
    ("rgba", "rgba(${1:red}, ${2:green}, ${3:blue}, ${4:alpha})", "Creates a Color from red, green, blue, and alpha values."),
    ("hsl", "hsl(${1:hue}, ${2:saturation}, ${3:lightness})", "Creates a Color from hue, saturation, and lightness values."),
    ("hsla", "hsla(${1:hue}, ${2:saturation}, ${3:lightness}, ${4:alpha})", "Creates a Color from hue, saturation, lightness, and alpha values."),
    ("hwb", "hwb(${1:hue} ${2:white} ${3:black})", "Creates a Color from hue, white, and black values."),
    ("lab", "lab(${1:lightness} ${2:a} ${3:b})", "Creates a Color from lightness, a, and b values."),
    ("lch", "lch(${1:lightness} ${2:chroma} ${3:hue})", "Creates a Color from lightness, chroma, and hue values."),
    ("oklab", "oklab(${1:lightness} ${2:a} ${3:b})", "Creates a Color from lightness, a, and b values."),
    ("oklch", "oklch(${1:lightness} ${2:chroma} ${3:hue})", "Creates a Color from lightness, chroma, and hue values."),
    ("color-mix", "color-mix(in ${1:srgb}, ${2:color}, ${3:color})", "Mixes two colors in a given color space."),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_colors() {
        assert_eq!(named("RebeccaPurple"), Some([0x66 as f32 / 255.0, 0x33 as f32 / 255.0, 0x99 as f32 / 255.0, 1.0]));
        assert_eq!(hex("#f00"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(hex("#ff000080").map(|c| (c[3] * 255.0).round()), Some(128.0));
        assert_eq!(hex("#ff00"), Some([1.0, 1.0, 0.0, 0.0]));
        assert!(hex("#ff0g").is_none());
        assert_eq!(function("rgb", "255, 0, 0"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(function("rgb", "255 0 0 / 50%"), Some([1.0, 0.0, 0.0, 0.5]));
        assert_eq!(function("hsl", "120, 100%, 50%"), Some([0.0, 1.0, 0.0, 1.0]));
    }
}
