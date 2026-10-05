//! The browser data for CSS (properties, at-rules, pseudo-classes and pseudo-elements with
//! MDN's descriptions), trimmed by `tools/update_data.py`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Deserialize, Debug, Default)]
pub struct Baseline {
    pub status: Option<String>,
    pub baseline_low_date: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ValueEntry {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Entry {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub browsers: Vec<String>,
    pub syntax: Option<String>,
    pub relevance: Option<i64>,
    /// "nonstandard", "experimental" or "obsolete".
    pub status: Option<String>,
    /// What values a property takes ("color", "length", "enum"...).
    #[serde(default)]
    pub restrictions: Vec<String>,
    pub baseline: Option<Baseline>,
    pub mdn: Option<String>,
    #[serde(default)]
    pub values: Vec<ValueEntry>,
}

impl Entry {
    pub fn relevance(&self) -> i64 {
        self.relevance.unwrap_or(50)
    }

    pub fn obsolete(&self) -> bool {
        self.status.as_deref() == Some("obsolete")
    }

    pub fn restricted_to(&self, kind: &str) -> bool {
        self.restrictions.iter().any(|r| r == kind)
    }

    /// The description: the text, browser support, syntax and the MDN link.
    pub fn markdown(&self) -> String {
        let mut parts = Vec::new();
        if let Some(d) = &self.description {
            parts.push(d.clone());
        }
        if let Some(support) = self.support() {
            parts.push(format!("_{support}_"));
        }
        if let Some(s) = &self.syntax {
            parts.push(format!("Syntax: {s}"));
        }
        if let Some(url) = &self.mdn {
            parts.push(format!("[MDN Reference]({url})"));
        }
        parts.join("\n\n")
    }

    /// "Widely available across major browsers (Baseline since 2015)", or the browsers.
    fn support(&self) -> Option<String> {
        if let Some(b) = &self.baseline {
            let year = b.baseline_low_date.as_deref().and_then(|d| d.get(..4));
            return Some(match (b.status.as_deref(), year) {
                (Some("high"), Some(y)) => format!("Widely available across major browsers (Baseline since {y})"),
                (Some("low"), Some(y)) => format!("Newly available across major browsers (Baseline since {y})"),
                (Some("false"), _) => "Limited availability across major browsers".to_string(),
                _ => return None,
            });
        }
        let names: Vec<String> = self.browsers.iter().filter_map(|b| browser_label(b)).collect();
        (!names.is_empty()).then(|| format!("({})", names.join(", ")))
    }
}

/// "FF16" → "Firefox 16".
fn browser_label(code: &str) -> Option<String> {
    let split = code.find(|c: char| c.is_ascii_digit()).unwrap_or(code.len());
    let (name, version) = code.split_at(split);
    let name = match name {
        "E" => "Edge",
        "FF" => "Firefox",
        "S" => "Safari",
        "C" => "Chrome",
        "IE" => "IE",
        "O" => "Opera",
        _ => return None,
    };
    Some(if version.is_empty() { name.to_string() } else { format!("{name} {version}") })
}

#[derive(Deserialize)]
struct Raw {
    properties: Vec<Entry>,
    #[serde(rename = "atDirectives")]
    at_directives: Vec<Entry>,
    #[serde(rename = "pseudoClasses")]
    pseudo_classes: Vec<Entry>,
    #[serde(rename = "pseudoElements")]
    pseudo_elements: Vec<Entry>,
}

pub struct Data {
    pub properties: Vec<Entry>,
    pub at_directives: Vec<Entry>,
    pub pseudo_classes: Vec<Entry>,
    pub pseudo_elements: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

impl Data {
    pub fn property(&self, name: &str) -> Option<&Entry> {
        self.by_name.get(&name.to_ascii_lowercase()).map(|&i| &self.properties[i])
    }

    pub fn at_directive(&self, name: &str) -> Option<&Entry> {
        self.at_directives.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    pub fn pseudo(&self, name: &str) -> Option<&Entry> {
        let list = if name.starts_with("::") { &self.pseudo_elements } else { &self.pseudo_classes };
        // `:nth-child(...)` is listed as `:nth-child()`.
        let base = name.split('(').next().unwrap_or(name);
        list.iter().find(|e| e.name.eq_ignore_ascii_case(name) || e.name.split('(').next().is_some_and(|n| n.eq_ignore_ascii_case(base)))
    }
}

pub fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| {
        let raw: Raw = serde_json::from_str(include_str!("../data/css-data.json")).expect("valid CSS data");
        let by_name = raw.properties.iter().enumerate().map(|(i, p)| (p.name.clone(), i)).collect();
        Data { properties: raw.properties, at_directives: raw.at_directives, pseudo_classes: raw.pseudo_classes, pseudo_elements: raw.pseudo_elements, by_name }
    })
}

/// HTML element names, for selectors.
pub const HTML_TAGS: &[&str] = &[
    "a", "abbr", "address", "area", "article", "aside", "audio", "b", "base", "bdi", "bdo", "blockquote", "body", "br",
    "button", "canvas", "caption", "cite", "code", "col", "colgroup", "data", "datalist", "dd", "del", "details", "dfn",
    "dialog", "div", "dl", "dt", "em", "embed", "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4",
    "h5", "h6", "head", "header", "hgroup", "hr", "html", "i", "iframe", "img", "input", "ins", "kbd", "label", "legend",
    "li", "link", "main", "map", "mark", "menu", "meta", "meter", "nav", "noscript", "object", "ol", "optgroup", "option",
    "output", "p", "param", "picture", "pre", "progress", "q", "rp", "rt", "ruby", "s", "samp", "script", "search",
    "section", "select", "slot", "small", "source", "span", "strong", "style", "sub", "summary", "sup", "svg", "table",
    "tbody", "td", "template", "textarea", "tfoot", "th", "thead", "time", "title", "tr", "track", "u", "ul", "var",
    "video", "wbr",
];

/// At-rules of SCSS and Less that CSS doesn't have.
pub const SCSS_AT_RULES: &[(&str, &str)] = &[
    ("@use", "Loads mixins, functions, and variables from other Sass stylesheets as 'modules', and combines CSS from multiple stylesheets together."),
    ("@forward", "Loads a Sass stylesheet and makes its mixins, functions, and variables available when this stylesheet is loaded with the @use rule."),
    ("@import", "Includes the content of a file."),
    ("@mixin", "Defines styles that can be re-used throughout the stylesheet with `@include`."),
    ("@include", "Includes the styles defined by another mixin into the current rule."),
    ("@function", "Defines complex operations that can be re-used throughout stylesheets."),
    ("@return", "Provides the value that serves as the result of a function call."),
    ("@extend", "Inherits the styles of another selector."),
    ("@at-root", "Causes one or more rules to be emitted at the root of the document, rather than being nested beneath their parent selectors."),
    ("@error", "Prints the value of an expression and stops compiling."),
    ("@warn", "Prints the value of an expression as a warning."),
    ("@debug", "Prints the value of an expression to the standard error output stream."),
    ("@if", "Includes the styles in the block if the condition is true."),
    ("@else", "Includes the styles in the block if no earlier condition was true."),
    ("@each", "Each value in a list or map is assigned to the variable, and the block is evaluated."),
    ("@for", "Repeats a block for a range of numbers."),
    ("@while", "While the condition is true, the block is evaluated."),
    ("@content", "Includes the content block passed to the mixin."),
];

pub const LESS_AT_RULES: &[(&str, &str)] = &[("@import", "Includes the content of a file."), ("@plugin", "Loads a JavaScript plugin.")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_entries() {
        let d = data();
        let color = d.property("color").unwrap();
        assert!(color.restricted_to("color"));
        assert!(color.markdown().contains("MDN Reference"));
        assert!(d.property("display").unwrap().values.iter().any(|v| v.name == "flex"));
        assert!(d.at_directive("@media").is_some());
        assert!(d.pseudo(":hover").is_some());
        assert!(d.pseudo(":nth-child(2n)").is_some());
        assert!(d.pseudo("::before").is_some());
    }
}
