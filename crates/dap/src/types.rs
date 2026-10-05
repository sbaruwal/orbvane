//! The Debug Adapter Protocol's data types the editor uses, parsed from JSON by hand.

use std::path::PathBuf;

use serde_json::Value;

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn int(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or(0)
}

fn list<T>(v: &Value, k: &str, parse: impl Fn(&Value) -> Option<T>) -> Vec<T> {
    v[k].as_array().map(|a| a.iter().filter_map(parse).collect()).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

impl Thread {
    /// The `threads` in a `threads` response body.
    pub fn parse_all(body: &Value) -> Vec<Thread> {
        list(body, "threads", |t| Some(Thread { id: t["id"].as_i64()?, name: text(t, "name") }))
    }
}

/// Where a frame's code is: a file, or (without a path) source the adapter can send.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Source {
    pub name: String,
    pub path: Option<PathBuf>,
    pub reference: i64,
}

impl Source {
    fn parse(v: &Value) -> Option<Source> {
        if !v.is_object() {
            return None;
        }
        Some(Source { name: text(v, "name"), path: v["path"].as_str().map(PathBuf::from), reference: int(v, "sourceReference") })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    pub source: Option<Source>,
    /// 1-based, 0 when there's no source.
    pub line: i64,
    pub column: i64,
    /// "normal", "label" or "subtle" (frames without debug info are shown dimmed).
    pub hint: String,
}

impl StackFrame {
    /// The frames in a `stackTrace` response body, and the thread's total number of frames.
    pub fn parse_all(body: &Value) -> (Vec<StackFrame>, Option<i64>) {
        let frames = list(body, "stackFrames", |f| {
            Some(StackFrame {
                id: f["id"].as_i64()?,
                name: text(f, "name"),
                source: Source::parse(&f["source"]),
                line: int(f, "line"),
                column: int(f, "column"),
                hint: text(f, "presentationHint"),
            })
        });
        (frames, body["totalFrames"].as_i64())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub name: String,
    pub reference: i64,
    /// Fetching its variables is slow (registers, globals): we leave these collapsed.
    pub expensive: bool,
}

impl Scope {
    pub fn parse_all(body: &Value) -> Vec<Scope> {
        list(body, "scopes", |s| {
            Some(Scope { name: text(s, "name"), reference: s["variablesReference"].as_i64()?, expensive: s["expensive"].as_bool().unwrap_or(false) })
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub ty: String,
    /// Non-zero when it has children (a struct's fields, a vector's items).
    pub reference: i64,
    /// An expression for it (for Add to Watch and Copy as Expression).
    pub evaluate_name: String,
}

impl Variable {
    pub fn parse_all(body: &Value) -> Vec<Variable> {
        list(body, "variables", |v| {
            Some(Variable {
                name: text(v, "name"),
                value: text(v, "value"),
                ty: text(v, "type"),
                reference: int(v, "variablesReference"),
                evaluate_name: text(v, "evaluateName"),
            })
        })
    }

    /// The result of an `evaluate` request, as a variable named `expression`.
    pub fn from_evaluate(expression: &str, body: &Value) -> Variable {
        Variable {
            name: expression.to_string(),
            value: text(body, "result"),
            ty: text(body, "type"),
            reference: int(body, "variablesReference"),
            evaluate_name: expression.to_string(),
        }
    }
}

/// A breakpoint as the adapter set it (from `setBreakpoints` or a `breakpoint` event).
#[derive(Clone, Debug, PartialEq)]
pub struct Breakpoint {
    pub id: Option<i64>,
    pub verified: bool,
    /// Where it really is (the adapter may move it to a line with code), 1-based.
    pub line: Option<i64>,
    pub message: String,
}

impl Breakpoint {
    pub fn parse(v: &Value) -> Breakpoint {
        Breakpoint { id: v["id"].as_i64(), verified: v["verified"].as_bool().unwrap_or(false), line: v["line"].as_i64(), message: text(v, "message") }
    }

    pub fn parse_all(body: &Value) -> Vec<Breakpoint> {
        list(body, "breakpoints", |b| Some(Breakpoint::parse(b)))
    }
}

/// The body of a `stopped` event.
#[derive(Clone, Debug, PartialEq)]
pub struct Stopped {
    /// "breakpoint", "step", "exception", "pause", "entry"...
    pub reason: String,
    pub description: String,
    pub text: String,
    pub thread: Option<i64>,
    pub all_threads: bool,
}

impl Stopped {
    pub fn parse(body: &Value) -> Stopped {
        Stopped {
            reason: text(body, "reason"),
            description: text(body, "description"),
            text: text(body, "text"),
            thread: body["threadId"].as_i64(),
            all_threads: body["allThreadsStopped"].as_bool().unwrap_or(false),
        }
    }
}

/// The body of an `output` event.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// "console", "stdout", "stderr", "important", "telemetry"...
    pub category: String,
    pub text: String,
}

impl Output {
    pub fn parse(body: &Value) -> Output {
        let category = body["category"].as_str().unwrap_or("console").to_string();
        Output { category, text: text(body, "output") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_frames_and_variables() {
        let body = json!({"stackFrames": [
            {"id": 1, "name": "dbgdemo::add", "line": 7, "column": 15, "source": {"name": "main.rs", "path": "/p/src/main.rs"}},
            {"id": 2, "name": "start", "line": 0, "column": 0, "presentationHint": "subtle"}
        ], "totalFrames": 2});
        let (frames, total) = StackFrame::parse_all(&body);
        assert_eq!(total, Some(2));
        assert_eq!(frames[0].source.as_ref().unwrap().path.as_deref(), Some(std::path::Path::new("/p/src/main.rs")));
        assert!(frames[1].source.is_none());
        assert_eq!(frames[1].hint, "subtle");

        let vars = Variable::parse_all(&json!({"variables": [
            {"name": "p", "value": "{x:3, y:4}", "type": "Point", "variablesReference": 5, "evaluateName": "p"},
            {"name": "total", "value": "0", "type": "i32", "variablesReference": 0}
        ]}));
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].reference, 5);
        assert_eq!(vars[1].ty, "i32");
    }

    #[test]
    fn parses_events() {
        let s = Stopped::parse(&json!({"reason": "breakpoint", "threadId": 3, "allThreadsStopped": true}));
        assert_eq!((s.reason.as_str(), s.thread, s.all_threads), ("breakpoint", Some(3), true));
        let o = Output::parse(&json!({"output": "hi\n"}));
        assert_eq!((o.category.as_str(), o.text.as_str()), ("console", "hi\n"));
        let b = Breakpoint::parse_all(&json!({"breakpoints": [{"verified": true, "line": 8, "id": 1}, {"verified": false, "message": "no code"}]}));
        assert_eq!(b[0].line, Some(8));
        assert!(!b[1].verified);
    }
}
