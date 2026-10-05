//! Debugs a Go program with Delve (`dlv dap`, which connects back to us over TCP). Skipped when
//! `dlv` or `go` isn't installed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dap::{Client, Incoming};
use serde_json::{json, Value};

fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    std::env::split_paths(&path).chain([PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin"), home.join("go/bin")]).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Waits for a message `done` accepts; messages before it are dropped, ones after it kept.
fn wait(client: &mut Client, mut done: impl FnMut(&Incoming) -> bool) -> Incoming {
    thread_local!(static LATER: std::cell::RefCell<Vec<Incoming>> = const { std::cell::RefCell::new(Vec::new()) });
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let mut batch = LATER.with(|l| std::mem::take(&mut *l.borrow_mut()));
        batch.extend(client.poll());
        let mut rest = batch.into_iter();
        while let Some(msg) = rest.next() {
            if let Incoming::Log(line) = &msg {
                eprintln!("dlv: {line}");
            }
            if done(&msg) {
                LATER.with(|l| l.borrow_mut().extend(rest));
                return msg;
            }
        }
        assert!(Instant::now() < deadline, "Delve went quiet");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn response(client: &mut Client, command: &str, args: Value) -> Value {
    let seq = client.request(command, args);
    match wait(client, |m| matches!(m, Incoming::Response { request_seq, .. } if *request_seq == seq)) {
        Incoming::Response { result: Ok(body), .. } => body,
        Incoming::Response { result: Err(e), .. } => panic!("{command} failed: {e}"),
        _ => unreachable!(),
    }
}

fn event(client: &mut Client, name: &str) -> Value {
    match wait(client, |m| matches!(m, Incoming::Event { event, .. } if event == name)) {
        Incoming::Event { body, .. } => body,
        _ => unreachable!(),
    }
}

#[test]
fn debugs_a_go_program() {
    let (Some(dlv), Some(_go)) = (find("dlv"), find("go")) else { return eprintln!("no dlv/go; skipped") };
    let dir = std::env::temp_dir().join(format!("delve-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module demo\n\ngo 1.21\n").unwrap();
    let main = dir.join("main.go");
    std::fs::write(&main, "package main\n\nimport \"fmt\"\n\nfunc add(a, b int) int {\n\tsum := a + b\n\treturn sum\n}\n\nfunc main() {\n\tfmt.Println(add(2, 3))\n}\n").unwrap();

    let waker: dap::Waker = Arc::new(|| {});
    let mut client = Client::spawn_client_addr(&dlv, &["dap".into(), "--client-addr={addr}".into()], &dir, "go", waker).unwrap();
    // `spawn_client_addr` sent `initialize` as request 1.
    wait(&mut client, |m| matches!(m, Incoming::Response { request_seq: 1, .. }));
    client.request("launch", json!({ "name": "Launch", "type": "go", "request": "launch", "mode": "debug", "program": dir }));
    event(&mut client, "initialized");
    let r = response(&mut client, "setBreakpoints", json!({ "source": { "path": main }, "breakpoints": [{ "line": 7 }] }));
    assert_eq!(r["breakpoints"][0]["verified"], true, "{r}");
    response(&mut client, "configurationDone", Value::Null);
    let stopped = event(&mut client, "stopped");
    assert_eq!(stopped["reason"], "breakpoint");
    let thread = stopped["threadId"].clone();
    let frames = response(&mut client, "stackTrace", json!({ "threadId": thread, "levels": 5 }));
    assert_eq!(frames["stackFrames"][0]["name"], "main.add");
    let scopes = response(&mut client, "scopes", json!({ "frameId": frames["stackFrames"][0]["id"] }));
    let vars = response(&mut client, "variables", json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }));
    let sum = vars["variables"].as_array().unwrap().iter().find(|v| v["name"] == "sum").unwrap().clone();
    assert_eq!(sum["value"], "5");
    response(&mut client, "continue", json!({ "threadId": thread }));
    // The program's own output arrives as output events.
    let out = event(&mut client, "output");
    assert_eq!((out["category"].as_str(), out["output"].as_str()), (Some("stdout"), Some("5\n")));
    event(&mut client, "terminated");
    response(&mut client, "disconnect", json!({}));
}
