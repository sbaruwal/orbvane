//! How JavaScript values read in the Variables view, hovers and the console, after js-debug:
//! `'text'`, `ƒ name(a, b)`, `(3) [1, 2, 3]`, `{a: 1, b: 'x'}`.

use serde_json::Value;

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n"))
}

/// `function add(a, b) { ... }` → `ƒ add(a, b)`.
fn function(description: &str) -> String {
    let head = description.split('{').next().unwrap_or(description).trim();
    let head = head.strip_prefix("async ").map_or(head.to_string(), |h| format!("async {h}"));
    if let Some(rest) = head.strip_prefix("function") {
        return format!("ƒ {}", rest.trim_start_matches('*').trim());
    }
    if let Some(rest) = head.strip_prefix("class") {
        return format!("class {}", rest.trim());
    }
    // Arrows and methods as written.
    let one_line = head.lines().next().unwrap_or("").trim();
    if one_line.contains("=>") { one_line.to_string() } else { format!("ƒ {one_line}") }
}

/// A property preview's value (`Runtime.PropertyPreview`).
fn property_preview(p: &Value) -> String {
    let value = p["value"].as_str().unwrap_or("");
    match (p["type"].as_str().unwrap_or(""), p["subtype"].as_str()) {
        ("string", _) => quote(value),
        ("function", _) => "ƒ".into(),
        ("object", Some("null")) => "null".into(),
        ("object", Some("array")) => value.to_string(),
        ("object", Some(_)) => value.to_string(),
        ("object", None) if value == "Object" => "{…}".into(),
        ("accessor", _) => "(...)".into(),
        _ => value.to_string(),
    }
}

/// An object with a preview: its first properties inline.
fn preview(o: &Value, p: &Value) -> String {
    let description = o["description"].as_str().unwrap_or("Object");
    let more = if p["overflow"].as_bool().unwrap_or(false) { ", …" } else { "" };
    if let Some(entries) = p["entries"].as_array() {
        // Map and Set.
        let items: Vec<String> = entries
            .iter()
            .map(|e| {
                let value = property_like(&e["value"]);
                match e.get("key") {
                    Some(k) if !k.is_null() => format!("{} => {value}", property_like(k)),
                    _ => value,
                }
            })
            .collect();
        return format!("{description} {{{}{more}}}", items.join(", "));
    }
    let props = p["properties"].as_array().cloned().unwrap_or_default();
    if p["subtype"] == "array" || o["subtype"] == "array" || o["subtype"] == "typedarray" {
        let len = description.rsplit_once('(').and_then(|(_, n)| n.strip_suffix(')')).unwrap_or("");
        let items: Vec<String> = props.iter().filter(|q| q["name"].as_str().is_some_and(|n| n.parse::<usize>().is_ok())).map(property_preview).collect();
        let prefix = if description.starts_with("Array(") { String::new() } else { format!("{} ", description.split('(').next().unwrap_or("")) };
        return format!("{prefix}({len}) [{}{more}]", items.join(", "));
    }
    let items: Vec<String> = props.iter().map(|q| format!("{}: {}", q["name"].as_str().unwrap_or(""), property_preview(q))).collect();
    let class = if description == "Object" { String::new() } else { format!("{description} ") };
    format!("{class}{{{}{more}}}", items.join(", "))
}

/// An `ObjectPreview` used as an entry's key or value.
fn property_like(p: &Value) -> String {
    match p["type"].as_str() {
        Some("string") => quote(p["description"].as_str().unwrap_or("")),
        _ => p["description"].as_str().unwrap_or("").to_string(),
    }
}

/// How a `Runtime.RemoteObject` reads.
pub fn describe(o: &Value) -> String {
    let description = o["description"].as_str();
    match o["type"].as_str().unwrap_or("") {
        "string" => quote(o["value"].as_str().unwrap_or("")),
        "undefined" => "undefined".into(),
        "number" | "bigint" => description.map(String::from).unwrap_or_else(|| o["value"].to_string()),
        "boolean" => o["value"].to_string(),
        "symbol" => description.unwrap_or("Symbol()").to_string(),
        "function" => function(description.unwrap_or("function")),
        "object" if o["subtype"] == "null" => "null".into(),
        "object" => match o.get("preview").filter(|p| !p.is_null()) {
            Some(p) if !matches!(o["subtype"].as_str(), Some("error" | "regexp" | "date" | "promise")) => preview(o, p),
            _ => description.unwrap_or("Object").to_string(),
        },
        _ => description.unwrap_or("").to_string(),
    }
}

/// A console argument: strings as they are, other values described.
pub fn console_arg(o: &Value) -> String {
    match o["type"].as_str() {
        Some("string") => o["value"].as_str().unwrap_or("").to_string(),
        _ => describe(o),
    }
}

/// `console.log` arguments as one line, with `%s %d %i %f %o %O %j %c` in the first one.
pub fn console_message(args: &[Value]) -> String {
    let mut rest = args.iter();
    let Some(first) = rest.next() else { return String::new() };
    let mut out = String::new();
    if first["type"] == "string" {
        let text = first["value"].as_str().unwrap_or("");
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.peek().copied() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some(spec @ ('s' | 'd' | 'i' | 'f' | 'o' | 'O' | 'j' | 'c')) => {
                    chars.next();
                    let Some(arg) = rest.next() else {
                        out.push('%');
                        out.push(spec);
                        continue;
                    };
                    match spec {
                        'c' => {}
                        'd' | 'i' => out.push_str(&arg["value"].as_f64().map_or("NaN".into(), |v| (v.trunc() as i64).to_string())),
                        's' => out.push_str(&console_arg(arg)),
                        _ => out.push_str(&describe(arg)),
                    }
                }
                _ => out.push('%'),
            }
        }
    } else {
        out = describe(first);
    }
    for arg in rest {
        out.push(' ');
        out.push_str(&console_arg(arg));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_values() {
        assert_eq!(describe(&json!({ "type": "string", "value": "it's" })), "'it\\'s'");
        assert_eq!(describe(&json!({ "type": "number", "value": 3, "description": "3" })), "3");
        assert_eq!(describe(&json!({ "type": "object", "subtype": "null", "value": null })), "null");
        assert_eq!(describe(&json!({ "type": "function", "description": "function add(a, b) {\n return a + b; }" })), "ƒ add(a, b)");
        assert_eq!(describe(&json!({ "type": "function", "description": "(x) => x * 2" })), "(x) => x * 2");
        let arr = json!({ "type": "object", "subtype": "array", "description": "Array(3)", "preview": {
            "type": "object", "subtype": "array", "overflow": false, "properties": [
                { "name": "0", "type": "number", "value": "1" }, { "name": "1", "type": "string", "value": "b" },
                { "name": "2", "type": "object", "value": "Object" } ] } });
        assert_eq!(describe(&arr), "(3) [1, 'b', {…}]");
        let obj = json!({ "type": "object", "className": "Object", "description": "Object", "preview": {
            "type": "object", "overflow": true, "properties": [ { "name": "a", "type": "number", "value": "1" } ] } });
        assert_eq!(describe(&obj), "{a: 1, …}");
        let point = json!({ "type": "object", "className": "Point", "description": "Point", "preview": {
            "type": "object", "overflow": false, "properties": [ { "name": "x", "type": "number", "value": "1" } ] } });
        assert_eq!(describe(&point), "Point {x: 1}");
    }

    #[test]
    fn formats_console_messages() {
        let args = [json!({ "type": "string", "value": "%s is %d%%" }), json!({ "type": "string", "value": "n" }), json!({ "type": "number", "value": 4.5 }), json!({ "type": "boolean", "value": true })];
        assert_eq!(console_message(&args), "n is 4% true");
    }
}
