//! JSON Schema validation (parts the settings, launch and tasks schemas use): which
//! schemas apply to which node, and what's wrong with the document. A port of the validator in
//! JSON language service, with its messages.

use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use crate::parse::{Doc, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub severity: Severity,
}

/// A schema that applies to a node (`inverted`: it applies through `not`).
#[derive(Clone, Copy)]
pub struct Match<'s> {
    pub node: usize,
    pub schema: &'s Value,
    pub inverted: bool,
}

#[derive(Default)]
struct Outcome {
    problems: Vec<Problem>,
    /// Properties the schema knows (how well an `anyOf` alternative fits).
    property_matches: usize,
    property_value_matches: usize,
    primary_value_matches: usize,
    enum_value_match: bool,
}

impl Outcome {
    fn merge(&mut self, other: Outcome) {
        self.problems.extend(other.problems);
    }

    /// Orders alternatives: fewer problems first, then the better fit.
    fn better_than(&self, other: &Outcome) -> std::cmp::Ordering {
        (self.problems.is_empty(), self.enum_value_match, self.property_value_matches, self.property_matches, self.primary_value_matches).cmp(&(
            other.problems.is_empty(),
            other.enum_value_match,
            other.property_value_matches,
            other.property_matches,
            other.primary_value_matches,
        ))
    }
}

/// Follows `$ref`s within the schema's own document (`#/definitions/x`, `#/$defs/x`, `#`).
pub fn resolve<'s>(root: &'s Value, mut schema: &'s Value) -> &'s Value {
    for _ in 0..32 {
        let Some(r) = schema.get("$ref").and_then(Value::as_str) else { break };
        let Some(pointer) = r.strip_prefix('#') else { break };
        let decoded = pointer.replace("%24", "$");
        match root.pointer(&decoded) {
            Some(target) => schema = target,
            None => break,
        }
    }
    schema
}

/// The alternatives a schema stands for: itself plus its `allOf`/`anyOf`/`oneOf` branches,
/// resolved and flattened.
pub fn alternatives<'s>(root: &'s Value, schema: &'s Value, out: &mut Vec<&'s Value>) {
    let schema = resolve(root, schema);
    if out.iter().any(|s| std::ptr::eq(*s, schema)) || out.len() > 200 {
        return;
    }
    out.push(schema);
    for key in ["allOf", "anyOf", "oneOf"] {
        for s in schema.get(key).and_then(Value::as_array).into_iter().flatten() {
            alternatives(root, s, out);
        }
    }
    for key in ["then", "else"] {
        if let Some(s) = schema.get(key) {
            alternatives(root, s, out);
        }
    }
}

/// The schema for property `key` of an object matching `schema` (None: the schema doesn't
/// describe it).
pub fn property_schema<'s>(root: &'s Value, schema: &'s Value, key: &str, regexes: &mut Regexes) -> Option<&'s Value> {
    let schema = resolve(root, schema);
    if let Some(s) = schema.get("properties").and_then(|p| p.get(key)) {
        return Some(s);
    }
    if let Some(patterns) = schema.get("patternProperties").and_then(Value::as_object) {
        for (pattern, s) in patterns {
            if regexes.get(pattern).is_some_and(|r| r.is_match(key)) {
                return Some(s);
            }
        }
    }
    schema.get("additionalProperties").filter(|s| s.is_object())
}

/// The schema for item `index` of an array matching `schema`.
pub fn item_schema<'s>(root: &'s Value, schema: &'s Value, index: usize) -> Option<&'s Value> {
    let schema = resolve(root, schema);
    match schema.get("items") {
        Some(Value::Array(tuple)) => tuple.get(index).or_else(|| schema.get("additionalItems").filter(|s| s.is_object())),
        Some(s @ Value::Object(_)) => Some(s),
        _ => schema.get("prefixItems").and_then(|p| p.get(index)),
    }
}

/// Compiled `pattern`s, cached (None: a pattern our regex engine doesn't support).
#[derive(Default)]
pub struct Regexes(HashMap<String, Option<Regex>>);

impl Regexes {
    pub fn get(&mut self, pattern: &str) -> Option<&Regex> {
        self.0.entry(pattern.to_string()).or_insert_with(|| Regex::new(pattern).ok()).as_ref()
    }
}

fn type_name(doc: &Doc, node: usize) -> &'static str {
    let n = doc.node(node);
    match n.kind {
        Kind::Object => "object",
        Kind::Array => "array",
        Kind::String => "string",
        Kind::Number => "number",
        Kind::Bool => "boolean",
        Kind::Null => "null",
        Kind::Property => "property",
    }
}

fn type_matches(doc: &Doc, node: usize, ty: &str) -> bool {
    let actual = type_name(doc, node);
    actual == ty || (ty == "integer" && actual == "number" && doc.node(node).text.parse::<f64>().is_ok_and(|n| n.fract() == 0.0))
}

fn show(v: &Value) -> String {
    v.to_string()
}

pub struct Validator<'d, 's> {
    doc: &'d Doc,
    root: &'s Value,
    pub regexes: Regexes,
}

impl<'d, 's> Validator<'d, 's> {
    pub fn new(doc: &'d Doc, root: &'s Value) -> Self {
        Self { doc, root, regexes: Regexes::default() }
    }

    /// Validates the document against the root schema: the problems, and every schema that
    /// applies to each node.
    pub fn run(&mut self) -> (Vec<Problem>, Vec<Match<'s>>) {
        let mut out = Outcome::default();
        let mut matches = Vec::new();
        if let Some(root) = self.doc.root {
            self.validate(root, self.root, &mut out, &mut matches);
        }
        (out.problems, matches)
    }

    fn problem(&self, out: &mut Outcome, node: usize, message: String) {
        let n = self.doc.node(node);
        out.problems.push(Problem { start: n.start, end: n.end, message, severity: Severity::Warning });
    }

    /// Where a problem about a node as a whole goes: its key if it's a property's value.
    fn key_or_node(&self, node: usize) -> usize {
        match self.doc.node(node).parent {
            Some(p) if self.doc.node(p).kind == Kind::Property => self.doc.node(p).children[0],
            _ => node,
        }
    }

    fn validate(&mut self, node: usize, schema: &'s Value, out: &mut Outcome, matches: &mut Vec<Match<'s>>) {
        let schema = resolve(self.root, schema);
        match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => return self.problem(out, node, "Matches a schema that is not allowed.".into()),
            Value::Object(_) => {}
            _ => return,
        }
        match self.doc.node(node).kind {
            Kind::Object => self.object(node, schema, out, matches),
            Kind::Array => self.array(node, schema, out, matches),
            Kind::String => self.string(node, schema, out),
            Kind::Number => self.number(node, schema, out),
            Kind::Property => {
                if let Some(v) = self.doc.property_value(node) {
                    return self.validate(v, schema, out, matches);
                }
            }
            Kind::Bool | Kind::Null => {}
        }
        self.common(node, schema, out, matches);
        matches.push(Match { node, schema, inverted: false });
    }

    fn common(&mut self, node: usize, schema: &'s Value, out: &mut Outcome, matches: &mut Vec<Match<'s>>) {
        let error_message = schema.get("errorMessage").and_then(Value::as_str);
        match schema.get("type") {
            Some(Value::String(ty)) if !type_matches(self.doc, node, ty) => {
                let message = error_message.map_or_else(|| format!("Incorrect type. Expected \"{ty}\"."), String::from);
                self.problem(out, node, message);
            }
            Some(Value::Array(types)) if !types.iter().filter_map(Value::as_str).any(|t| type_matches(self.doc, node, t)) => {
                let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
                let message = error_message.map_or_else(|| format!("Incorrect type. Expected one of {}.", names.join(", ")), String::from);
                self.problem(out, node, message);
            }
            _ => {}
        }
        for s in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
            self.validate(node, s, out, matches);
        }
        if let Some(not) = schema.get("not") {
            let mut sub = Outcome::default();
            let mut sub_matches = Vec::new();
            self.validate(node, not, &mut sub, &mut sub_matches);
            if sub.problems.is_empty() {
                let message = error_message.unwrap_or("Matches a schema that is not allowed.").to_string();
                self.problem(out, node, message);
            }
            matches.extend(sub_matches.into_iter().map(|m| Match { inverted: !m.inverted, ..m }));
        }
        for (key, one) in [("anyOf", false), ("oneOf", true)] {
            if let Some(alts) = schema.get(key).and_then(Value::as_array) {
                self.alternatives(node, alts, one, out, matches);
            }
        }
        if let Some(cond) = schema.get("if") {
            let mut sub = Outcome::default();
            let mut sub_matches = Vec::new();
            self.validate(node, cond, &mut sub, &mut sub_matches);
            matches.extend(sub_matches);
            let branch = if sub.problems.is_empty() { schema.get("then") } else { schema.get("else") };
            if let Some(branch) = branch {
                self.validate(node, branch, out, matches);
            }
        }

        let value = || self.doc.value(node);
        if let Some(options) = schema.get("enum").and_then(Value::as_array) {
            let v = value();
            if options.iter().any(|o| *o == v) {
                out.enum_value_match = true;
            } else {
                let list: Vec<String> = options.iter().map(show).collect();
                let message = error_message.map_or_else(|| format!("Value is not accepted. Valid values: {}.", list.join(", ")), String::from);
                self.problem(out, node, message);
            }
        }
        if let Some(c) = schema.get("const") {
            if *c == value() {
                out.enum_value_match = true;
            } else {
                let message = error_message.map_or_else(|| format!("Value must be {}.", show(c)), String::from);
                self.problem(out, node, message);
            }
        }
        let deprecation = schema.get("deprecationMessage").or_else(|| schema.get("markdownDeprecationMessage")).and_then(Value::as_str);
        if let Some(message) = deprecation {
            let at = self.key_or_node(node);
            let n = self.doc.node(at);
            out.problems.push(Problem { start: n.start, end: n.end, message: message.to_string(), severity: Severity::Warning });
        }
    }

    /// `anyOf`/`oneOf`: the best fitting alternative's problems and matches count; equally
    /// good ones are merged (their properties are all offered).
    fn alternatives(&mut self, node: usize, alts: &'s [Value], one_of: bool, out: &mut Outcome, matches: &mut Vec<Match<'s>>) {
        let mut best: Option<(Outcome, Vec<Match<'s>>)> = None;
        let mut valid = 0;
        for alt in alts {
            let mut sub = Outcome::default();
            let mut sub_matches = Vec::new();
            self.validate(node, alt, &mut sub, &mut sub_matches);
            if sub.problems.is_empty() {
                valid += 1;
            }
            match &mut best {
                None => best = Some((sub, sub_matches)),
                Some((b, b_matches)) => match sub.better_than(b) {
                    std::cmp::Ordering::Greater => best = Some((sub, sub_matches)),
                    std::cmp::Ordering::Equal => {
                        b_matches.extend(sub_matches);
                        b.enum_value_match |= sub.enum_value_match;
                    }
                    std::cmp::Ordering::Less => {}
                },
            }
        }
        if one_of && valid > 1 {
            let n = self.doc.node(node);
            out.problems.push(Problem { start: n.start, end: n.start + 1, message: "Matches multiple schemas when only one must validate.".into(), severity: Severity::Warning });
        }
        if let Some((b, b_matches)) = best {
            out.property_matches += b.property_matches;
            out.property_value_matches += b.property_value_matches;
            out.primary_value_matches += b.primary_value_matches;
            out.enum_value_match |= b.enum_value_match;
            out.merge(b);
            matches.extend(b_matches);
        }
    }

    fn number(&mut self, node: usize, schema: &'s Value, out: &mut Outcome) {
        let Ok(n) = self.doc.node(node).text.parse::<f64>() else { return };
        let get = |k: &str| schema.get(k).and_then(Value::as_f64);
        if let Some(m) = get("multipleOf").filter(|m| *m != 0.0) {
            if (n / m).fract().abs() > 1e-9 {
                self.problem(out, node, format!("Value is not divisible by {m}."));
            }
        }
        // Draft 4 has boolean `exclusiveMinimum`; later drafts give the bound.
        let exclusive = |k: &str, bound: &str| match schema.get(k) {
            Some(Value::Bool(true)) => get(bound),
            Some(v) => v.as_f64(),
            None => None,
        };
        let shown = |v: f64| Value::from(v).to_string().trim_end_matches(".0").to_string();
        if let Some(min) = exclusive("exclusiveMinimum", "minimum") {
            if n <= min {
                return self.problem(out, node, format!("Value is below the exclusive minimum of {}.", shown(min)));
            }
        }
        if let Some(max) = exclusive("exclusiveMaximum", "maximum") {
            if n >= max {
                return self.problem(out, node, format!("Value is above the exclusive maximum of {}.", shown(max)));
            }
        }
        if let Some(min) = get("minimum").filter(|_| schema.get("exclusiveMinimum") != Some(&Value::Bool(true))) {
            if n < min {
                self.problem(out, node, format!("Value is below the minimum of {}.", shown(min)));
            }
        }
        if let Some(max) = get("maximum").filter(|_| schema.get("exclusiveMaximum") != Some(&Value::Bool(true))) {
            if n > max {
                self.problem(out, node, format!("Value is above the maximum of {}.", shown(max)));
            }
        }
    }

    fn string(&mut self, node: usize, schema: &'s Value, out: &mut Outcome) {
        let text = self.doc.node(node).text.clone();
        let len = text.chars().count() as u64;
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64).filter(|&m| len < m) {
            self.problem(out, node, format!("String is shorter than the minimum length of {min}."));
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64).filter(|&m| len > m) {
            self.problem(out, node, format!("String is longer than the maximum length of {max}."));
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            if self.regexes.get(pattern).is_some_and(|r| !r.is_match(&text)) {
                let custom = schema.get("patternErrorMessage").or_else(|| schema.get("errorMessage")).and_then(Value::as_str);
                let message = custom.map_or_else(|| format!("String does not match the pattern of \"{pattern}\"."), String::from);
                self.problem(out, node, message);
            }
        }
        if schema.get("format").and_then(Value::as_str) == Some("color-hex") && !is_hex_color(&text) {
            self.problem(out, node, "Invalid color format. Use #RGB, #RGBA, #RRGGBB or #RRGGBBAA.".into());
        }
    }

    fn array(&mut self, node: usize, schema: &'s Value, out: &mut Outcome, matches: &mut Vec<Match<'s>>) {
        let items = self.doc.node(node).children.clone();
        match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (i, &item) in items.iter().enumerate() {
                    match tuple.get(i) {
                        Some(s) => {
                            let mut sub = Outcome::default();
                            self.validate(item, s, &mut sub, matches);
                            out.primary_value_matches += sub.problems.is_empty() as usize;
                            out.merge(sub);
                        }
                        None => match schema.get("additionalItems") {
                            Some(Value::Bool(false)) => {
                                self.problem(out, node, format!("Array has too many items according to schema. Expected {} or fewer.", tuple.len()));
                                break;
                            }
                            Some(s @ Value::Object(_)) => self.validate(item, s, out, matches),
                            _ => {}
                        },
                    }
                }
            }
            Some(s @ (Value::Object(_) | Value::Bool(_))) => {
                for &item in &items {
                    let mut sub = Outcome::default();
                    self.validate(item, s, &mut sub, matches);
                    out.primary_value_matches += sub.problems.is_empty() as usize;
                    out.merge(sub);
                }
            }
            _ => {}
        }
        if let Some(contains) = schema.get("contains") {
            let found = items.iter().any(|&item| {
                let mut sub = Outcome::default();
                self.validate(item, contains, &mut sub, &mut Vec::new());
                sub.problems.is_empty()
            });
            if !found {
                let message = schema.get("errorMessage").and_then(Value::as_str).unwrap_or("Array does not contain required item.").to_string();
                self.problem(out, node, message);
            }
        }
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64).filter(|&m| (items.len() as u64) < m) {
            self.problem(out, node, format!("Array has too few items. Expected {min} or more."));
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64).filter(|&m| (items.len() as u64) > m) {
            self.problem(out, node, format!("Array has too many items. Expected {max} or fewer."));
        }
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            let values: Vec<Value> = items.iter().map(|&i| self.doc.value(i)).collect();
            if values.iter().enumerate().any(|(i, v)| values[..i].contains(v)) {
                self.problem(out, node, "Array has duplicate items.".into());
            }
        }
    }

    fn object(&mut self, node: usize, schema: &'s Value, out: &mut Outcome, matches: &mut Vec<Match<'s>>) {
        let props = self.doc.node(node).children.clone();
        let mut unprocessed: Vec<usize> = props.clone();
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !props.iter().any(|&p| self.doc.key(p) == key) {
                    let at = self.key_or_node(node);
                    let n = self.doc.node(at);
                    let end = if at == node { n.start + 1 } else { n.end };
                    out.problems.push(Problem { start: n.start, end, message: format!("Missing property \"{key}\"."), severity: Severity::Warning });
                }
            }
        }
        let not_allowed = |v: &Validator, out: &mut Outcome, p: usize| {
            let key = v.doc.node(p).children[0];
            let message = schema
                .get("errorMessage")
                .and_then(Value::as_str)
                .map_or_else(|| format!("Property {} is not allowed.", v.doc.key(p)), String::from);
            let n = v.doc.node(key);
            out.problems.push(Problem { start: n.start, end: n.end, message, severity: Severity::Warning });
        };
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for &p in &props {
                let Some(s) = properties.get(self.doc.key(p)) else { continue };
                unprocessed.retain(|&u| u != p);
                if *s == Value::Bool(false) {
                    not_allowed(self, out, p);
                    continue;
                }
                out.property_matches += 1;
                if let Some(v) = self.doc.property_value(p) {
                    let mut sub = Outcome::default();
                    self.validate(v, s, &mut sub, matches);
                    out.property_value_matches += sub.problems.is_empty() as usize;
                    out.merge(sub);
                }
            }
        }
        if let Some(patterns) = schema.get("patternProperties").and_then(Value::as_object) {
            for (pattern, s) in patterns {
                for &p in &props {
                    if !self.regexes.get(pattern).is_some_and(|r| r.is_match(self.doc.key(p))) {
                        continue;
                    }
                    unprocessed.retain(|&u| u != p);
                    if let Some(v) = self.doc.property_value(p) {
                        let mut sub = Outcome::default();
                        self.validate(v, s, &mut sub, matches);
                        out.property_value_matches += sub.problems.is_empty() as usize;
                        out.merge(sub);
                    }
                }
            }
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                for &p in &unprocessed {
                    not_allowed(self, out, p);
                }
            }
            Some(s @ Value::Object(_)) => {
                for &p in &unprocessed {
                    if let Some(v) = self.doc.property_value(p) {
                        self.validate(v, s, out, matches);
                    }
                }
            }
            _ => {}
        }
        let count = props.len() as u64;
        if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64).filter(|&m| count > m) {
            self.problem(out, node, format!("Object has more properties than limit of {max}."));
        }
        if let Some(min) = schema.get("minProperties").and_then(Value::as_u64).filter(|&m| count < m) {
            self.problem(out, node, format!("Object has fewer properties than the required number of {min}"));
        }
        if let Some(names) = schema.get("propertyNames") {
            for &p in &props {
                let key = self.doc.node(p).children[0];
                self.validate(key, names, out, &mut Vec::new());
            }
        }
    }
}

pub fn is_hex_color(s: &str) -> bool {
    s.strip_prefix('#').is_some_and(|h| matches!(h.len(), 3 | 4 | 6 | 8) && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Problems that need no schema: duplicate keys.
pub fn duplicate_keys(doc: &Doc) -> Vec<Problem> {
    let mut out = Vec::new();
    for n in doc.nodes.iter().filter(|n| n.kind == Kind::Object) {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for &p in &n.children {
            let key = doc.key(p);
            if let Some(first) = seen.insert(key, p) {
                for at in [first, p] {
                    let k = doc.node(doc.node(at).children[0]);
                    if !out.iter().any(|e: &Problem| e.start == k.start) {
                        out.push(Problem { start: k.start, end: k.end, message: "Duplicate object key".into(), severity: Severity::Warning });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problems(text: &str, schema: Value) -> Vec<(String, String)> {
        let doc = Doc::parse(text);
        let (problems, _) = Validator::new(&doc, &schema).run();
        problems.into_iter().map(|p| (text[p.start..p.end].to_string(), p.message)).collect()
    }

    #[test]
    fn validates_types_enums_and_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "size": { "type": "integer", "minimum": 6, "maximum": 100 },
                "mode": { "enum": ["on", "off"] },
                "name": { "$ref": "#/definitions/name" },
                "old": { "deprecationMessage": "Use name." }
            },
            "required": ["mode"],
            "additionalProperties": false,
            "definitions": { "name": { "type": "string", "pattern": "^[a-z]+$" } }
        });
        let got = problems(r#"{ "size": 4.5, "name": "X", "old": 1, "extra": true }"#, schema);
        let want = [
            ("{", "Missing property \"mode\"."),
            ("4.5", "Value is below the minimum of 6."),
            ("4.5", "Incorrect type. Expected \"integer\"."),
            ("\"X\"", "String does not match the pattern of \"^[a-z]+$\"."),
            ("\"old\"", "Use name."),
            ("\"extra\"", "Property extra is not allowed."),
        ];
        let want: Vec<(String, String)> = want.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(got, want);
        let got = problems(r#"{ "mode": "maybe" }"#, json!({ "properties": { "mode": { "enum": ["on", "off"] } } }));
        assert_eq!(got, [("\"maybe\"".to_string(), "Value is not accepted. Valid values: \"on\", \"off\".".to_string())]);
    }

    #[test]
    fn picks_the_matching_alternative() {
        let schema = json!({ "items": { "oneOf": [
            { "properties": { "type": { "const": "a" }, "x": { "type": "number" } }, "required": ["type"] },
            { "properties": { "type": { "const": "b" }, "y": { "type": "string" } }, "required": ["type"] }
        ] } });
        assert!(problems(r#"[{ "type": "b", "y": "s" }, { "type": "a", "x": 1 }]"#, schema.clone()).is_empty());
        // The alternative that fits best reports its problems.
        assert_eq!(problems(r#"[{ "type": "b", "y": 1 }]"#, schema), [("1".to_string(), "Incorrect type. Expected \"string\".".to_string())]);
    }

    #[test]
    fn finds_duplicate_keys() {
        let doc = Doc::parse(r#"{ "a": 1, "a": 2 }"#);
        assert_eq!(duplicate_keys(&doc).len(), 2);
    }
}
