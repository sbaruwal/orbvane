//! End-to-end test against a real rust-analyzer. Skipped when it isn't installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp::{path_to_uri, Client, Incoming, Position};
use serde_json::json;

fn rust_analyzer() -> Option<PathBuf> {
    let out = std::process::Command::new("rust-analyzer").arg("--version").output().ok()?;
    out.status.success().then(|| PathBuf::from("rust-analyzer"))
}

fn wait_for(client: &mut Client, timeout: Duration, mut f: impl FnMut(&Incoming) -> bool) -> Option<Incoming> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for msg in client.poll() {
            if std::env::var("LSP_DEBUG").is_ok() {
                eprintln!("<< {msg:?}");
            }
            if f(&msg) {
                return Some(msg);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn response(client: &mut Client, id: i64) -> Result<serde_json::Value, String> {
    match wait_for(client, Duration::from_secs(60), |m| matches!(m, Incoming::Response { id: i, .. } if *i == id)) {
        Some(Incoming::Response { result, .. }) => result,
        _ => panic!("no response to request {id}"),
    }
}

/// Sends a request until it succeeds with a non-null result that `parse` accepts.
/// rust-analyzer returns null or "content modified" while the workspace is still loading.
fn retry<T>(client: &mut Client, method: &str, params: serde_json::Value, parse: impl Fn(&serde_json::Value) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(if std::env::var("LSP_DEBUG").is_ok() { 15 } else { 120 });
    loop {
        let id = client.request(method, params.clone());
        if let Ok(v) = response(client, id) {
            if let Some(t) = parse(&v) {
                return t;
            }
        }
        assert!(Instant::now() < deadline, "{method} never succeeded");
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn diagnostics_hover_definition_completion() {
    let Some(ra) = rust_analyzer() else {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("orbvane-lsp-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    let src = "fn helper() -> u32 { 1 }\n\nfn main() {\n    let x: u32 = \"no\";\n    helper();\n    let v: Vec<u32> = Vec::new();\n    v.\n}\n";
    let main = dir.join("src/main.rs");
    std::fs::write(&main, src).unwrap();

    let mut client = Client::spawn("rust-analyzer", &ra, &[], &dir, serde_json::Value::Null, serde_json::Value::Null, Arc::new(|| {})).unwrap();
    let uri = path_to_uri(&main);
    client.notify("textDocument/didOpen", json!({
        "textDocument": { "uri": uri, "languageId": "rust", "version": 1, "text": src }
    }));

    // The type error (and the incomplete `v.`) produce diagnostics.
    let diags = wait_for(&mut client, Duration::from_secs(120), |m| match m {
        Incoming::Notification { method, params } if method == "textDocument/publishDiagnostics" => {
            // Diagnostics arrive in waves (syntax errors first); wait for the type error.
            lsp::parse_diagnostics(params).is_some_and(|(p, d)| p.ends_with("main.rs") && d.iter().any(|d| d.range.start.line == 3))
        }
        _ => false,
    });
    let Some(Incoming::Notification { params, .. }) = diags else { panic!("no diagnostics") };
    let (_, diags) = lsp::parse_diagnostics(&params).unwrap();
    assert!(diags.iter().any(|d| d.range.start.line == 3), "expected an error on line 4: {diags:?}");

    // Hover over `helper` in the call on line 5.
    let pos = Position { line: 4, character: 6 };
    let at = json!({ "textDocument": { "uri": uri }, "position": pos.to_json() });
    let hover = retry(&mut client, "textDocument/hover", at.clone(), lsp::parse_hover);
    assert!(hover.contains("fn helper() -> u32"), "{hover}");

    // Definition of `helper` is on line 1.
    let locs = retry(&mut client, "textDocument/definition", at, |v| Some(lsp::parse_locations(v)).filter(|l| !l.is_empty()));
    assert_eq!(locs[0].range.start.line, 0);
    assert!(locs[0].path.ends_with("main.rs"));

    // Completion after `v.` offers Vec methods.
    let (items, _) = retry(&mut client, "textDocument/completion", json!({
        "textDocument": { "uri": uri }, "position": { "line": 6, "character": 6 },
        "context": { "triggerKind": 2, "triggerCharacter": "." }
    // (While the standard library is still loading, the answer is a few items without Vec's.)
    }), |v| Some(lsp::parse_completions(v)).filter(|(items, _)| items.iter().any(|i| i.label.starts_with("push"))));
    assert!(items.iter().any(|i| i.label.starts_with("push")), "no push in {} items", items.len());

    // Format on type: `=` typed in a `let` without its `;` gets one.
    let src2 = "fn main() {\n    let y = 2\n}\n";
    client.notify("textDocument/didChange", json!({
        "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [{ "text": src2 }]
    }));
    let edits = retry(&mut client, "textDocument/onTypeFormatting", json!({
        "textDocument": { "uri": uri }, "position": { "line": 1, "character": 11 }, "ch": "=",
        "options": { "tabSize": 4, "insertSpaces": true }
    }), |v| Some(lsp::parse_text_edits(v)).filter(|e| !e.is_empty()));
    assert!(edits.iter().any(|e| e.new_text == ";"), "{edits:?}");

    client.shutdown();
    let _ = std::fs::remove_dir_all(Path::new(&dir));
}
