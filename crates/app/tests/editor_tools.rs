//! The agent's view of the editor's tools: the `orbvane` binary as an MCP server on stdio,
//! forwarding to the editor's `acp::mcp::Bridge`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[test]
fn the_helper_speaks_mcp_on_stdio() {
    let exe = Path::new(env!("CARGO_BIN_EXE_orbvane"));
    let bridge = acp::mcp::Bridge::start(Arc::new(|| {})).unwrap();
    let config = bridge.server_config("orbvane", exe);
    let mut helper = Command::new(config["command"].as_str().unwrap())
        .args(config["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()))
        .envs(config["env"].as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap(), e["value"].as_str().unwrap())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = helper.stdin.take().unwrap();
    let mut stdout = BufReader::new(helper.stdout.take().unwrap());
    let messages = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "diagnostics", "arguments": {} } }),
    ];
    for m in &messages {
        writeln!(stdin, "{m}").unwrap();
    }
    let mut read = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let init = read();
    assert_eq!(init["id"], 1);
    assert!(init["result"]["capabilities"]["tools"].is_object());
    // The call reaches the editor; its answer goes back to the agent.
    let deadline = Instant::now() + Duration::from_secs(10);
    let req = loop {
        if let Some(r) = bridge.poll() {
            break r;
        }
        assert!(Instant::now() < deadline, "the call didn't reach the editor");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(req.method, "tools/call");
    assert_eq!(req.params["name"], "diagnostics");
    req.answer_text("No problems in the workspace.", false);
    let answer = read();
    assert_eq!(answer["id"], 2);
    assert_eq!(answer["result"]["content"][0]["text"], "No problems in the workspace.");
    drop(stdin);
    assert!(helper.wait().unwrap().success());
}
