//! The browser data for HTML (elements, attributes and their values, with MDN's
//! descriptions), trimmed by `crates/css/tools/update_data.py`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Value {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Attribute {
    pub name: String,
    pub description: Option<String>,
    /// A named set of values (`valueSets`); "v" means the attribute takes no value.
    #[serde(rename = "valueSet")]
    pub value_set: Option<String>,
    #[serde(default)]
    pub values: Vec<Value>,
    pub mdn: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Tag {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub attributes: Vec<Attribute>,
    pub mdn: Option<String>,
    #[serde(default)]
    pub void: bool,
}

#[derive(Deserialize)]
struct ValueSet {
    name: String,
    values: Vec<Value>,
}

#[derive(Deserialize)]
struct Raw {
    tags: Vec<Tag>,
    #[serde(rename = "globalAttributes")]
    global_attributes: Vec<Attribute>,
    #[serde(rename = "valueSets")]
    value_sets: Vec<ValueSet>,
}

pub struct Data {
    pub tags: Vec<Tag>,
    pub global_attributes: Vec<Attribute>,
    value_sets: HashMap<String, Vec<Value>>,
}

/// Description and MDN link as markdown.
pub fn markdown(description: Option<&str>, mdn: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        parts.push(d.to_string());
    }
    if let Some(url) = mdn {
        parts.push(format!("[MDN Reference]({url})"));
    }
    parts.join("\n\n")
}

impl Tag {
    pub fn markdown(&self) -> String {
        markdown(self.description.as_deref(), self.mdn.as_deref())
    }
}

impl Attribute {
    pub fn markdown(&self) -> String {
        markdown(self.description.as_deref(), self.mdn.as_deref())
    }
}

impl Data {
    pub fn tag(&self, name: &str) -> Option<&Tag> {
        self.tags.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// The attributes an element can have: its own, then the global ones.
    pub fn attributes(&self, tag: &str) -> impl Iterator<Item = (&Attribute, bool)> {
        let own = self.tag(tag).map(|t| t.attributes.as_slice()).unwrap_or_default();
        own.iter().map(|a| (a, true)).chain(self.global_attributes.iter().map(|a| (a, false)))
    }

    pub fn attribute(&self, tag: &str, name: &str) -> Option<&Attribute> {
        self.attributes(tag).map(|(a, _)| a).find(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// An attribute's values: its own, or its value set's.
    pub fn values<'a>(&'a self, attribute: &'a Attribute) -> &'a [Value] {
        if !attribute.values.is_empty() {
            return &attribute.values;
        }
        attribute.value_set.as_ref().and_then(|s| self.value_sets.get(s)).map(Vec::as_slice).unwrap_or_default()
    }
}

pub fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| {
        let raw: Raw = serde_json::from_str(include_str!("../data/html-data.json")).expect("valid HTML data");
        Data { tags: raw.tags, global_attributes: raw.global_attributes, value_sets: raw.value_sets.into_iter().map(|s| (s.name, s.values)).collect() }
    })
}

/// Elements without content or end tag.
pub fn is_void(tag: &str) -> bool {
    const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param", "source", "track", "wbr"];
    VOID.iter().any(|v| v.eq_ignore_ascii_case(tag)) || data().tag(tag).is_some_and(|t| t.void)
}

/// Whether starting a `next` element ends an open `open` element (HTML's optional end tags).
pub fn closed_by(open: &str, next: &str) -> bool {
    const BLOCKS: &[&str] = &[
        "address", "article", "aside", "blockquote", "details", "div", "dl", "fieldset", "figcaption", "figure", "footer", "form", "h1",
        "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "main", "menu", "nav", "ol", "p", "pre", "section", "table", "ul",
    ];
    let next = next.to_ascii_lowercase();
    match open.to_ascii_lowercase().as_str() {
        "p" => BLOCKS.contains(&next.as_str()),
        "li" => next == "li",
        "dt" | "dd" => next == "dt" || next == "dd",
        "tr" => next == "tr" || next == "tbody" || next == "tfoot",
        "td" | "th" => next == "td" || next == "th" || next == "tr",
        "option" => next == "option" || next == "optgroup",
        "optgroup" => next == "optgroup",
        "thead" | "tbody" => next == "tbody" || next == "tfoot",
        "rt" | "rp" => next == "rt" || next == "rp",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_tags_and_attributes() {
        let d = data();
        assert!(d.tag("div").unwrap().markdown().contains("MDN Reference"));
        let input_type = d.attribute("input", "type").unwrap();
        assert!(d.values(input_type).iter().any(|v| v.name == "checkbox"));
        assert!(d.attribute("div", "class").is_some());
        assert!(is_void("br") && !is_void("div"));
        assert!(closed_by("p", "div") && closed_by("li", "li") && !closed_by("div", "p"));
    }
}
