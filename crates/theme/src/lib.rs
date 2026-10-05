//! Workbench colors using theme color keys (`editor.background`, `tab.activeForeground`, ...),
//! so color theme files can be loaded as-is.
//!
//! A theme is a color theme file (colors + `tokenColors`). Colors the file
//! doesn't set come from the color registry defaults (`registry.rs`), evaluated against
//! the theme the same way. The built-in themes are the standard theme files.

mod registry;
mod registry_expr;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use registry_expr::{eval, Rgba};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    pub const fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    /// Parses `#RGB`, `#RGBA`, `#RRGGBB` or `#RRGGBBAA`.
    pub fn hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#')?;
        let digit = |i: usize| u8::from_str_radix(&s[i..i + 1], 16).ok();
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        match s.len() {
            3 | 4 => {
                let a = if s.len() == 4 { digit(3)? * 17 } else { 255 };
                Some(Self::rgba8(digit(0)? * 17, digit(1)? * 17, digit(2)? * 17, a))
            }
            6 | 8 => {
                let a = if s.len() == 8 { byte(6)? } else { 255 };
                Some(Self::rgba8(byte(0)?, byte(2)?, byte(4)?, a))
            }
            _ => None,
        }
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    pub fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// Syntax token categories the highlighter emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Plain,
    Keyword,
    ControlKeyword,
    String,
    Comment,
    Number,
    Type,
    Function,
    Macro,
    Variable,
    Constant,
    Attribute,
    Punctuation,
    Lifetime,
    // Kinds only semantic highlighting tells apart (from language servers).
    Namespace,
    Parameter,
    Property,
    EnumMember,
    TypeParameter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    Dark,
    Light,
    HighContrastDark,
    HighContrastLight,
}

impl ThemeKind {
    fn index(self) -> usize {
        self as usize
    }

    pub fn is_dark(self) -> bool {
        matches!(self, ThemeKind::Dark | ThemeKind::HighContrastDark)
    }

    /// From a theme file's `type` field ("dark", "light", "hc", "hcLight").
    pub fn from_type(t: &str) -> Self {
        match t {
            "light" | "vs" => ThemeKind::Light,
            "hc" | "hcDark" | "hc-black" => ThemeKind::HighContrastDark,
            "hcLight" | "hc-light" => ThemeKind::HighContrastLight,
            _ => ThemeKind::Dark,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeSource {
    /// One of the theme files compiled into the app.
    Builtin(&'static str),
    File(PathBuf),
}

/// A theme that can be picked, without loading it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub kind: ThemeKind,
    pub source: ThemeSource,
}

/// The built-in theme files: Orbvane's own.
const BUILTIN_FILES: &[(&str, &str)] = &[
    ("orbvane_night.json", include_str!("../themes/orbvane_night.json")),
    ("orbvane_day.json", include_str!("../themes/orbvane_day.json")),
    ("orbvane_dark.json", include_str!("../themes/orbvane_dark.json")),
];

const BUILTIN_THEMES: &[(&str, ThemeKind, &str)] = &[
    ("Orbvane Night", ThemeKind::Dark, "orbvane_night.json"),
    ("Orbvane Day", ThemeKind::Light, "orbvane_day.json"),
    ("Orbvane Dark", ThemeKind::Dark, "orbvane_dark.json"),
];

pub const DEFAULT_THEME: &str = "Orbvane Night";

/// The themes compiled into the app.
pub fn builtin_themes() -> Vec<ThemeInfo> {
    BUILTIN_THEMES
        .iter()
        .map(|&(name, kind, file)| ThemeInfo { name: name.into(), kind, source: ThemeSource::Builtin(file) })
        .collect()
}

/// color theme files (`*.json`) in `dir`, named by their `name` field.
pub fn user_themes(dir: &Path) -> Vec<ThemeInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut themes: Vec<ThemeInfo> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|path| {
            let json = parse_jsonc(&std::fs::read_to_string(&path).ok()?).ok()?;
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            let name = json.get("name").and_then(|v| v.as_str()).map_or(stem, str::to_string);
            let kind = ThemeKind::from_type(json.get("type").and_then(|v| v.as_str()).unwrap_or("dark"));
            Some(ThemeInfo { name, kind, source: ThemeSource::File(path) })
        })
        .collect();
    themes.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    themes
}

fn parse_jsonc(src: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(&strip_jsonc(src)).map_err(|e| e.to_string())
}

/// A token color rule.
struct Rule {
    selectors: Vec<String>,
    foreground: Color,
}

/// A theme file with its `include` chain merged (colors and token rules, parent first).
#[derive(Default)]
struct RawTheme {
    colors: HashMap<String, Color>,
    rules: Vec<Rule>,
    /// `semanticHighlighting`: whether language servers' semantic tokens color the text.
    semantic: Option<bool>,
    /// `semanticTokenColors`: selector ("variable.readonly", "*.mutable") -> color.
    semantic_colors: Vec<(String, Color)>,
}

impl RawTheme {
    fn read(src: &str, source: &ThemeSource, depth: usize) -> Result<Self, String> {
        let json = parse_jsonc(src)?;
        let mut raw = RawTheme::default();
        if let Some(include) = json.get("include").and_then(|v| v.as_str()).filter(|i| !i.trim().is_empty()) {
            if depth > 8 {
                return Err("theme includes are nested too deeply".into());
            }
            let (text, source) = resolve_include(include, source)?;
            raw = RawTheme::read(&text, &source, depth + 1)?;
        }
        if let Some(colors) = json.get("colors").and_then(|v| v.as_object()) {
            for (key, value) in colors {
                match value.as_str().and_then(Color::hex) {
                    Some(c) => raw.colors.insert(key.clone(), c),
                    None => raw.colors.remove(key),
                };
            }
        }
        for rule in json.get("tokenColors").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(fg) = rule.pointer("/settings/foreground").and_then(|v| v.as_str()).and_then(Color::hex) else { continue };
            let selectors: Vec<String> = match rule.get("scope") {
                Some(serde_json::Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
                Some(serde_json::Value::Array(a)) => {
                    a.iter().filter_map(|v| v.as_str()).flat_map(|s| s.split(',')).map(|s| s.trim().to_string()).collect()
                }
                // A rule without a scope sets the default foreground.
                _ => vec![String::new()],
            };
            raw.rules.push(Rule { selectors, foreground: fg });
        }
        if let Some(on) = json.get("semanticHighlighting").and_then(|v| v.as_bool()) {
            raw.semantic = Some(on);
        }
        for (sel, value) in json.get("semanticTokenColors").and_then(|v| v.as_object()).into_iter().flatten() {
            // A color, or a style object with a foreground.
            let color = value.as_str().or_else(|| value.get("foreground").and_then(|f| f.as_str())).and_then(Color::hex);
            if let Some(color) = color {
                raw.semantic_colors.retain(|(s, _)| s != sel);
                raw.semantic_colors.push((sel.clone(), color));
            }
        }
        Ok(raw)
    }

    /// The best rule's color for `scope` (e.g. "keyword.control.rust"): the most specific
    /// matching selector wins, and later rules win ties, like other scope-based themes.
    fn token_color(&self, scope: &str) -> Option<Color> {
        let mut best: Option<(usize, Color)> = None;
        for rule in &self.rules {
            for sel in &rule.selectors {
                // Descendant selectors ("source.go keyword") need scope context we don't have.
                if sel.is_empty() || sel.contains(' ') {
                    continue;
                }
                let matches = scope == sel || scope.strip_prefix(sel.as_str()).is_some_and(|r| r.starts_with('.'));
                let specificity = sel.split('.').count();
                if matches && best.is_none_or(|(s, _)| specificity >= s) {
                    best = Some((specificity, rule.foreground));
                }
            }
        }
        best.map(|(_, c)| c)
    }
}

fn resolve_include(include: &str, from: &ThemeSource) -> Result<(String, ThemeSource), String> {
    let file = include.trim_start_matches("./");
    if let ThemeSource::File(path) = from {
        let target = path.parent().unwrap_or(Path::new(".")).join(include);
        if let Ok(text) = std::fs::read_to_string(&target) {
            return Ok((text, ThemeSource::File(target)));
        }
    }
    BUILTIN_FILES
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(name, text)| (text.to_string(), ThemeSource::Builtin(name)))
        .ok_or_else(|| format!("included theme not found: {include}"))
}

/// Representative token scopes for our token kinds, most specific first.
const TOKEN_SCOPES: &[(Token, &[&str])] = &[
    (Token::Keyword, &["storage.type.rust", "keyword.other.rust", "keyword"]),
    (Token::ControlKeyword, &["keyword.control.rust"]),
    (Token::String, &["string.quoted.double.rust"]),
    (Token::Comment, &["comment.line.double-slash.rust"]),
    (Token::Number, &["constant.numeric.decimal.rust"]),
    (Token::Type, &["entity.name.type.rust"]),
    (Token::Function, &["entity.name.function.rust"]),
    (Token::Macro, &["entity.name.function.macro.rust"]),
    (Token::Variable, &["variable.other.rust"]),
    (Token::Constant, &["variable.other.constant.rust"]),
    (Token::Attribute, &["entity.other.attribute-name.rust"]),
    (Token::Punctuation, &["punctuation.separator.rust"]),
    (Token::Lifetime, &["storage.modifier.lifetime.rust"]),
    // The default token scopes for semantic token types.
    (Token::Namespace, &["entity.name.namespace"]),
    (Token::Parameter, &["variable.parameter"]),
    (Token::Property, &["variable.other.property"]),
    (Token::EnumMember, &["variable.other.enummember"]),
    (Token::TypeParameter, &["entity.name.type.parameter"]),
];

pub struct Theme {
    pub name: String,
    pub kind: ThemeKind,
    colors: HashMap<String, Color>,
    tokens: HashMap<Token, Color>,
    /// The theme's `semanticHighlighting` (false when it doesn't say).
    pub semantic_highlighting: bool,
    semantic_colors: Vec<(String, Color)>,
}

impl Theme {
    /// The default theme (`DEFAULT_THEME`).
    pub fn default_theme() -> Self {
        let info = builtin_themes().into_iter().find(|t| t.name == DEFAULT_THEME).expect("default theme");
        Self::load(&info).expect("built-in theme parses")
    }

    pub fn load(info: &ThemeInfo) -> Result<Self, String> {
        let text = match &info.source {
            ThemeSource::Builtin(file) => {
                BUILTIN_FILES.iter().find(|(n, _)| n == file).map(|(_, t)| t.to_string()).ok_or("unknown theme")?
            }
            ThemeSource::File(path) => std::fs::read_to_string(path).map_err(|e| e.to_string())?,
        };
        let raw = RawTheme::read(&text, &info.source, 0)?;
        Ok(Self::from_raw(info.name.clone(), info.kind, raw))
    }

    /// Loads a color theme file, taking its name and kind from the file.
    pub fn load_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let json = parse_jsonc(&text)?;
        let name = json.get("name").and_then(|v| v.as_str()).unwrap_or("Custom").to_string();
        let kind = ThemeKind::from_type(json.get("type").and_then(|v| v.as_str()).unwrap_or("dark"));
        let raw = RawTheme::read(&text, &ThemeSource::File(path.to_path_buf()), 0)?;
        Ok(Self::from_raw(name, kind, raw))
    }

    fn from_raw(name: String, kind: ThemeKind, raw: RawTheme) -> Self {
        let mut memo: HashMap<String, Option<Rgba>> = HashMap::new();
        for (id, _) in registry::DEFAULTS {
            resolve(id, &raw.colors, kind, &mut memo, &mut Vec::new());
        }
        let mut colors: HashMap<String, Color> =
            memo.into_iter().filter_map(|(id, c)| Some((id, c?.to_color()))).collect();
        colors.extend(raw.colors.iter().map(|(k, v)| (k.clone(), *v)));

        let fg = colors.get("editor.foreground").copied().unwrap_or(Color::rgba8(0xCC, 0xCC, 0xCC, 255));
        let mut tokens: HashMap<Token, Color> = TOKEN_SCOPES
            .iter()
            .filter_map(|(token, scopes)| Some((*token, scopes.iter().find_map(|s| raw.token_color(s))?)))
            .collect();
        tokens.insert(Token::Plain, fg);
        // Kinds the theme doesn't color fall back to the nearest kind it does (macros are
        // functions to most themes, parameters and properties are variables...).
        for (kind, like) in [
            (Token::Macro, Token::Function),
            (Token::Namespace, Token::Type),
            (Token::TypeParameter, Token::Type),
            (Token::Parameter, Token::Variable),
            (Token::Property, Token::Variable),
            (Token::EnumMember, Token::Constant),
        ] {
            if !tokens.contains_key(&kind) {
                if let Some(c) = tokens.get(&like).copied() {
                    tokens.insert(kind, c);
                }
            }
        }
        Self { name, kind, colors, tokens, semantic_highlighting: raw.semantic.unwrap_or(false), semantic_colors: raw.semantic_colors }
    }

    pub fn is_dark(&self) -> bool {
        self.kind.is_dark()
    }

    /// Looks up a theme color key. A registered key the theme leaves unset (no default for
    /// this kind of theme) is transparent; an unknown key renders magenta so it's easy to spot.
    pub fn color(&self, key: &str) -> Color {
        match self.colors.get(key) {
            Some(c) => *c,
            None if registry_index(key).is_some() => Color::TRANSPARENT,
            None => Color::rgba8(255, 0, 255, 255),
        }
    }

    /// The color for `key`, if the theme (or its defaults) sets one.
    pub fn color_opt(&self, key: &str) -> Option<Color> {
        self.colors.get(key).copied()
    }

    /// The theme's `semanticTokenColors` color for a semantic token of `ty` with `modifiers`:
    /// The matching selector ("type", "type.mod", "*.mod") with the most modifiers wins.
    pub fn semantic_color(&self, ty: &str, modifiers: &[&str]) -> Option<Color> {
        let mut best: Option<(usize, Color)> = None;
        for (sel, color) in &self.semantic_colors {
            let mut parts = sel.split(':').next().unwrap_or("").split('.');
            let t = parts.next().unwrap_or("");
            let mods: Vec<&str> = parts.collect();
            if (t == ty || t == "*") && mods.iter().all(|m| modifiers.contains(m)) && best.is_none_or(|(n, _)| mods.len() >= n) {
                best = Some((mods.len(), *color));
            }
        }
        best.map(|(_, c)| c)
    }

    pub fn token(&self, token: Token) -> Color {
        self.tokens.get(&token).copied().unwrap_or_else(|| self.color("editor.foreground"))
    }
}

fn registry_index(id: &str) -> Option<usize> {
    registry::DEFAULTS.binary_search_by(|(k, _)| k.cmp(&id)).ok()
}

/// Resolves a color id: the theme's value, else the registry default for the
/// theme's kind (which may refer to other colors).
fn resolve(
    id: &str,
    theme: &HashMap<String, Color>,
    kind: ThemeKind,
    memo: &mut HashMap<String, Option<Rgba>>,
    stack: &mut Vec<String>,
) -> Option<Rgba> {
    if let Some(c) = theme.get(id) {
        return Some(Rgba::from_color(*c));
    }
    if let Some(c) = memo.get(id) {
        return *c;
    }
    if stack.iter().any(|s| s == id) {
        return None; // a cycle in the defaults
    }
    let expr = &registry::DEFAULTS[registry_index(id)?].1[kind.index()];
    stack.push(id.to_string());
    let defines = |x: &str| theme.contains_key(x);
    let value = eval(expr, &mut |x| resolve(x, theme, kind, memo, stack), &defines);
    stack.pop();
    memo.insert(id.to_string(), value);
    value
}

/// Removes `//` and `/* */` comments and trailing commas (theme and settings files are
/// JSONC). Comments become spaces so byte offsets and line numbers are preserved.
pub fn strip_jsonc(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let mut in_string = false;
    // Pass 1: blank out comments.
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
            continue;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
            for (j, b) in out.iter_mut().enumerate().take(end).skip(i) {
                if bytes[j] != b'\n' {
                    *b = b' ';
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    // Pass 2: drop commas directly before a closing bracket.
    let mut in_string = false;
    let mut i = 0;
    while i < out.len() {
        let c = out[i];
        if in_string {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if c == b',' {
            let next = out[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}') | Some(b']') | None) {
                out[i] = b' ';
            }
        }
        i += 1;
    }
    // Only ASCII bytes were replaced (by spaces), so this is still valid UTF-8.
    String::from_utf8(out).expect("valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!(Color::hex("#fff"), Some(Color::rgba8(255, 255, 255, 255)));
        assert_eq!(Color::hex("#0078D4"), Some(Color::rgba8(0, 0x78, 0xD4, 255)));
        assert_eq!(Color::hex("#00000080").unwrap().a, 128.0 / 255.0);
    }

    #[test]
    fn builtin_themes_resolve() {
        let themes = builtin_themes();
        let load = |name: &str| Theme::load(themes.iter().find(|t| t.name == name).unwrap()).unwrap();
        let night = Theme::default_theme();
        assert_eq!(night.name, "Orbvane Night");
        assert!(night.is_dark());
        assert_eq!(night.color("editor.background"), Color::hex("#15131F").unwrap());
        assert_eq!(night.token(Token::Keyword), Color::hex("#A98AFF").unwrap());
        assert_eq!(night.token(Token::ControlKeyword), Color::hex("#EE8FE0").unwrap());
        assert_eq!(night.token(Token::Macro), Color::hex("#F4A261").unwrap());
        assert_eq!(night.token(Token::Lifetime), Color::hex("#EE8FE0").unwrap());
        let day = load("Orbvane Day");
        assert!(!day.is_dark());
        assert_eq!(day.color("editor.background"), Color::hex("#FCFBFF").unwrap());
        assert_eq!(day.token(Token::String), Color::hex("#2E7D50").unwrap());

        let graphite = load("Orbvane Dark");
        assert!(graphite.is_dark());
        assert_eq!(graphite.color("editor.background"), Color::hex("#161615").unwrap());
        assert_eq!(graphite.color("sideBar.background"), Color::hex("#111111").unwrap());
        assert_eq!(graphite.token(Token::Keyword), Color::hex("#6DA7EC").unwrap());
        // Brackets stay neutral; Night leaves the switcher's well unset.
        assert_eq!(graphite.color("editorBracketHighlight.foreground1"), Color::hex("#A3A199").unwrap());
        assert_eq!(night.color("activityBarTop.background"), Color::TRANSPARENT);
        // Registry defaults, evaluated against the theme.
        assert_ne!(night.color("list.hoverBackground"), Color::TRANSPARENT);
        assert_ne!(night.color("scrollbarSlider.background"), Color::TRANSPARENT);
        // Unset registered colors are transparent, unknown ones magenta.
        assert_eq!(night.color("contrastBorder"), Color::TRANSPARENT);
        assert_eq!(night.color("no.such.color"), Color::rgba8(255, 0, 255, 255));
    }

    #[test]
    fn user_theme_files() {
        let dir = std::env::temp_dir().join(format!("orbvane-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A user theme can build on a built-in one and override colors and token rules.
        std::fs::write(
            dir.join("mine.json"),
            r##"{ "name": "Mine", "type": "light", "include": "./orbvane_day.json",
                 "colors": { "editor.background": "#FFFFF0" },
                 "tokenColors": [ { "scope": "keyword.control, storage.type", "settings": { "foreground": "#FF0000" } } ] }"##,
        )
        .unwrap();
        std::fs::write(dir.join("broken.json"), "{ not json").unwrap();
        let themes = user_themes(&dir);
        assert_eq!(themes.len(), 1);
        assert_eq!((themes[0].name.as_str(), themes[0].kind), ("Mine", ThemeKind::Light));
        let t = Theme::load(&themes[0]).unwrap();
        assert_eq!(t.color("editor.background"), Color::hex("#FFFFF0").unwrap());
        assert_eq!(t.color("sideBar.background"), Color::hex("#F4F2FA").unwrap());
        assert_eq!(t.token(Token::ControlKeyword), Color::hex("#FF0000").unwrap());
        assert_eq!(t.token(Token::String), Color::hex("#2E7D50").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strips_jsonc() {
        let src = "{ // c\n \"a\": \"#fff\", /* x */ \"b\": [1,2,],\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"], "#fff");
    }
}
