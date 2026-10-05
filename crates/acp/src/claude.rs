//! An Agent Client Protocol agent in front of Claude Code: runs `claude -p` with JSON in and
//! out (`--input-format stream-json --output-format stream-json`, one message per line) and
//! translates. Started with `Client::in_process`, so the editor sees an ordinary agent.
//!
//! - `session/new` starts `claude` in the folder, with the editor's MCP servers
//!   (`--mcp-config`), asking us before it uses a tool (`--permission-prompt-tool stdio`; the
//!   permission mode is always `default`, whatever the user's settings say), and sends it the
//!   control protocol's `initialize`. Each `session/prompt` is a user message, answered when
//!   the `result` for it arrives; `session/cancel` is an `interrupt` control request.
//! - Its streamed events become session updates: text and thinking deltas; tool uses become
//!   tool calls (titled by tool, with diffs for edits), their results end them; `TodoWrite` is
//!   the plan.
//! - Its `can_use_tool` control requests become `session/request_permission`. "Allow for this
//!   chat" is remembered per tool (per command for Bash).
//!
//! Claude Code reads and writes files on disk itself (the editor saves before each message and
//! reloads what changed). An expired sign-in answers the prompt with the sign-in error code, so
//! the editor offers to sign in.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use serde_json::{json, Value};

use crate::bridge::{Lines, Options};

const INTERNAL: i64 = -32603;
const NOT_FOUND: i64 = -32601;
/// The code `session/new` and `session/prompt` fail with when the user has to sign in.
pub const AUTH_REQUIRED: i64 = -32000;
/// How many lines of a command's output go in its tool call.
const OUTPUT_LINES: usize = 12;

enum Event {
    Client(Value),
    ClientGone,
    Claude(Value),
    ClaudeGone,
}

impl From<Option<Value>> for Event {
    fn from(line: Option<Value>) -> Self {
        line.map_or(Event::ClaudeGone, Event::Claude)
    }
}

/// A `can_use_tool` request, now a permission question to the editor.
struct Asked {
    request_id: Value,
    tool: String,
    input: Value,
    /// What "for this chat" remembers.
    rule: String,
}

struct Bridge {
    to_client: Sender<Value>,
    log: Sender<String>,
    events: Sender<Event>,
    options: Options,
    claude: Option<Lines>,
    session: Option<String>,
    sessions: u32,
    /// The editor's `session/new` waiting for the control protocol's `initialize`.
    starting: Option<Value>,
    /// The editor's `session/prompt` waiting for its `result`.
    prompt: Option<Value>,
    /// Whether the turn was interrupted (its result then means "cancelled").
    interrupted: bool,
    next_control: u32,
    next_client_id: i64,
    asked: HashMap<i64, Asked>,
    /// Tools (or Bash commands) allowed for this chat.
    allowed: HashSet<String>,
    /// Messages whose text streamed (their full copy adds nothing), the message streaming
    /// now, and the one whose text was written last (a new one starts a new paragraph).
    streamed: HashSet<String>,
    message: Option<String>,
    last_text: Option<String>,
    /// Tool uses shown, by id: (tool name, its input), for their results.
    tools: HashMap<String, (String, Value)>,
    version: String,
}

/// Runs the bridge until the editor closes its end. See `Client::in_process`.
pub fn serve(options: Options, version: String, rx: Receiver<Value>, tx: Sender<Value>, log: Sender<String>) {
    let (events, inbox) = mpsc::channel();
    {
        let events = events.clone();
        thread::spawn(move || {
            for msg in rx {
                if events.send(Event::Client(msg)).is_err() {
                    return;
                }
            }
            let _ = events.send(Event::ClientGone);
        });
    }
    let mut b = Bridge {
        to_client: tx,
        log,
        events,
        options,
        claude: None,
        session: None,
        sessions: 0,
        starting: None,
        prompt: None,
        interrupted: false,
        next_control: 0,
        next_client_id: 0,
        asked: HashMap::new(),
        allowed: HashSet::new(),
        streamed: HashSet::new(),
        message: None,
        last_text: None,
        tools: HashMap::new(),
        version,
    };
    for event in inbox {
        match event {
            Event::Client(msg) => b.from_client(msg),
            Event::Claude(msg) => b.from_claude(msg),
            Event::ClientGone => break,
            Event::ClaudeGone => b.claude_gone(),
        }
    }
    if let Some(c) = b.claude.take() {
        c.end();
    }
}

impl Bridge {
    fn client(&self, msg: Value) {
        let _ = self.to_client.send(msg);
    }

    fn reply(&self, id: Value, result: Value) {
        self.client(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn reply_error(&self, id: Value, code: i64, message: &str) {
        self.client(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn update(&self, update: Value) {
        let Some(session) = &self.session else { return };
        self.client(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } }));
    }

    /// Reply text from message `message`.
    fn agent_text(&mut self, message: &str, text: &str) {
        let mut text = text.to_string();
        if self.last_text.as_deref().is_some_and(|m| m != message) {
            text.insert_str(0, "\n\n");
        }
        self.last_text = Some(message.to_string());
        self.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }));
    }

    fn claude_send(&mut self, msg: Value) {
        let Some(c) = &mut self.claude else { return };
        if !c.send(&msg) {
            let _ = self.events.send(Event::ClaudeGone);
        }
    }

    fn control(&mut self, request: Value) {
        self.next_control += 1;
        let id = format!("orbvane-{}", self.next_control);
        self.claude_send(json!({ "type": "control_request", "request_id": id, "request": request }));
    }

    // ------------------------------------------------------------------ the editor

    fn from_client(&mut self, msg: Value) {
        let method = msg["method"].as_str().map(str::to_string);
        match (method.as_deref(), msg.get("id").cloned()) {
            (Some(method), Some(id)) => self.client_request(method, id, &msg["params"]),
            (Some("session/cancel"), None) => {
                if self.prompt.is_some() {
                    self.interrupted = true;
                    self.control(json!({ "subtype": "interrupt" }));
                }
            }
            (None, Some(id)) => self.client_answer(id, &msg),
            _ => {}
        }
    }

    fn client_request(&mut self, method: &str, id: Value, params: &Value) {
        match method {
            "initialize" => self.reply(
                id,
                json!({
                    "protocolVersion": crate::PROTOCOL_VERSION,
                    "agentCapabilities": { "loadSession": false, "promptCapabilities": { "embeddedContext": true, "image": false, "audio": false } },
                    "agentInfo": { "name": "claude-code", "title": "Claude Code", "version": self.version },
                    "authMethods": [],
                }),
            ),
            "session/new" => self.new_session(id, params),
            "session/prompt" => {
                if self.claude.is_none() || self.prompt.is_some() || self.starting.is_some() {
                    return self.reply_error(id, INTERNAL, "Claude Code is still answering the last message.");
                }
                let text = crate::codex::prompt_text(&params["prompt"]);
                self.prompt = Some(id);
                self.interrupted = false;
                self.last_text = None;
                self.message = None;
                let message = json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "text", "text": text }] }, "parent_tool_use_id": null, "session_id": "" });
                self.claude_send(message);
            }
            "authenticate" => self.reply_error(id, INTERNAL, "Sign in to Claude Code from the agent menu (it runs `claude auth login`)."),
            _ => self.reply_error(id, NOT_FOUND, &format!("{method} isn't supported")),
        }
    }

    /// A new chat: a new `claude` (the old one ends), with the editor's MCP servers.
    fn new_session(&mut self, id: Value, params: &Value) {
        if let Some(c) = self.claude.take() {
            c.end();
        }
        self.allowed.clear();
        self.tools.clear();
        self.asked.clear();
        self.prompt = None;
        let mut args = self.options.args.clone();
        args.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--permission-mode",
                "default",
            ]
            .map(String::from),
        );
        if let Some(servers) = mcp_config(&params["mcpServers"]) {
            args.extend(["--mcp-config".to_string(), servers.to_string()]);
        }
        let mut options = Options { program: self.options.program.clone(), args, cwd: self.options.cwd.clone(), env: self.options.env.clone() };
        if let Some(cwd) = params["cwd"].as_str() {
            options.cwd = cwd.into();
        }
        match Lines::spawn(&options, self.events.clone(), self.log.clone()) {
            Ok(lines) => {
                self.claude = Some(lines);
                self.sessions += 1;
                self.session = Some(format!("claude-{}", self.sessions));
                self.starting = Some(id);
                self.control(json!({ "subtype": "initialize", "hooks": null }));
            }
            Err(e) => self.reply_error(id, INTERNAL, &format!("Couldn't start Claude Code: {e}")),
        }
    }

    /// The editor answered a permission question.
    fn client_answer(&mut self, id: Value, msg: &Value) {
        let Some(asked) = id.as_i64().and_then(|id| self.asked.remove(&id)) else { return };
        let outcome = &msg["result"]["outcome"];
        let option = if outcome["outcome"].as_str() == Some("selected") { outcome["optionId"].as_str().unwrap_or("") } else { "cancelled" };
        if option == "allow_always" {
            self.allowed.insert(asked.rule.clone());
        }
        let response = match option {
            "allow" | "allow_always" => json!({ "behavior": "allow", "updatedInput": asked.input }),
            "cancelled" => json!({ "behavior": "deny", "message": "The user stopped this.", "interrupt": true }),
            _ => json!({ "behavior": "deny", "message": format!("The user didn't allow {}.", asked.tool) }),
        };
        self.claude_send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": asked.request_id, "response": response } }));
    }

    // ------------------------------------------------------------------ Claude Code

    fn claude_gone(&mut self) {
        self.claude = None;
        let _ = self.log.send("Claude Code exited.".into());
        if let Some(id) = self.starting.take() {
            self.reply_error(id, INTERNAL, "Claude Code stopped while starting. The Output panel's Assistant channel shows what it printed.");
        }
        if let Some(id) = self.prompt.take() {
            self.reply_error(id, INTERNAL, "Claude Code stopped. Send a message to start a new chat.");
        }
    }

    fn from_claude(&mut self, msg: Value) {
        match msg["type"].as_str() {
            Some("control_response") => {
                // The answer to `initialize` (others: interrupts) means the session is ready.
                if let Some(id) = self.starting.take() {
                    let ok = msg["response"]["subtype"].as_str() != Some("error");
                    match (ok, &self.session) {
                        (true, Some(session)) => self.reply(id, json!({ "sessionId": session })),
                        _ => {
                            let e = msg["response"]["error"].as_str().unwrap_or("Claude Code didn't start.");
                            self.reply_error(id, INTERNAL, e);
                        }
                    }
                }
            }
            Some("control_request") => self.claude_asks(&msg),
            Some("stream_event") if msg["parent_tool_use_id"].is_null() => self.stream_event(&msg["event"]),
            Some("assistant") if msg["parent_tool_use_id"].is_null() => self.assistant_message(&msg),
            Some("user") => self.tool_results(&msg["message"]["content"]),
            Some("result") => self.result(&msg),
            Some("system") => {
                if msg["subtype"].as_str() == Some("api_retry") {
                    let _ = self.log.send(format!("Claude Code: retrying ({})", msg["error"].as_str().unwrap_or("")));
                }
            }
            _ => {}
        }
    }

    fn stream_event(&mut self, e: &Value) {
        match e["type"].as_str() {
            Some("message_start") => self.message = e["message"]["id"].as_str().map(String::from),
            Some("content_block_delta") => {
                let d = &e["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let message = self.message.clone().unwrap_or_default();
                        self.streamed.insert(message.clone());
                        self.agent_text(&message, d["text"].as_str().unwrap_or(""));
                    }
                    Some("thinking_delta") => {
                        let text = d["thinking"].as_str().unwrap_or("").to_string();
                        self.update(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text } }));
                    }
                    _ => {}
                }
            }
            Some("message_stop") => self.message = None,
            _ => {}
        }
    }

    /// A whole message: its tool uses become tool calls (and its text, if it didn't stream).
    fn assistant_message(&mut self, msg: &Value) {
        let m = &msg["message"];
        let id = m["id"].as_str().unwrap_or("").to_string();
        let streamed = self.streamed.contains(&id);
        let error = msg["error"].as_str().is_some();
        for block in m["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") if !streamed && !error => {
                    let text = block["text"].as_str().unwrap_or("");
                    if !text.is_empty() {
                        self.agent_text(&id, text);
                    }
                }
                Some("tool_use") => self.tool_use(block),
                _ => {}
            }
        }
    }

    fn tool_use(&mut self, block: &Value) {
        let id = block["id"].as_str().unwrap_or("").to_string();
        let name = block["name"].as_str().unwrap_or("").to_string();
        let input = block["input"].clone();
        if self.tools.contains_key(&id) {
            return;
        }
        self.tools.insert(id.clone(), (name.clone(), input.clone()));
        if name == "TodoWrite" {
            return self.update(json!({ "sessionUpdate": "plan", "entries": plan_entries(&input) }));
        }
        let (title, kind) = describe(&name, &input);
        let mut update = json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind, "status": "in_progress" });
        let diffs = edit_diffs(&name, &input);
        if !diffs.is_empty() {
            update["content"] = json!(diffs);
        }
        self.update(update);
    }

    fn tool_results(&mut self, content: &Value) {
        for block in content.as_array().into_iter().flatten().filter(|b| b["type"].as_str() == Some("tool_result")) {
            let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
            let Some((name, _)) = self.tools.get(&id).cloned() else { continue };
            if name == "TodoWrite" {
                continue;
            }
            let failed = block["is_error"].as_bool() == Some(true);
            let mut update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": if failed { "failed" } else { "completed" } });
            if name == "Bash" || failed {
                let text = result_text(&block["content"]);
                if !text.trim().is_empty() {
                    update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": crate::codex::tail(&text, OUTPUT_LINES) } }]);
                }
            }
            self.update(update);
        }
    }

    fn result(&mut self, msg: &Value) {
        self.message = None;
        self.streamed.clear();
        let Some(id) = self.prompt.take() else { return };
        let text = msg["result"].as_str().unwrap_or("");
        if self.interrupted {
            return self.reply(id, json!({ "stopReason": "cancelled" }));
        }
        let failed = msg["is_error"].as_bool() == Some(true) || msg["subtype"].as_str().is_some_and(|s| s.starts_with("error"));
        if !failed {
            return self.reply(id, json!({ "stopReason": "end_turn" }));
        }
        match msg["subtype"].as_str() {
            Some("error_max_turns") => self.reply(id, json!({ "stopReason": "max_turn_requests" })),
            _ if is_auth_error(msg) => self.reply_error(id, AUTH_REQUIRED, "Claude Code's sign-in has expired. Sign in again, then send your message."),
            _ => self.reply_error(id, INTERNAL, if text.is_empty() { "Claude Code stopped with an error." } else { text }),
        }
    }

    /// Claude Code asks to use a tool: allowed for this chat already, or a question.
    fn claude_asks(&mut self, msg: &Value) {
        let request_id = msg["request_id"].clone();
        let r = &msg["request"];
        if r["subtype"].as_str() != Some("can_use_tool") {
            let response = json!({ "subtype": "error", "request_id": request_id, "error": "Not supported" });
            return self.claude_send(json!({ "type": "control_response", "response": response }));
        }
        let tool = r["tool_name"].as_str().unwrap_or("").to_string();
        let input = r["input"].clone();
        let rule = if tool == "Bash" { format!("Bash:{}", input["command"].as_str().unwrap_or("")) } else { tool.clone() };
        if self.allowed.contains(&rule) {
            let response = json!({ "behavior": "allow", "updatedInput": input });
            return self.claude_send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": response } }));
        }
        let Some(session) = self.session.clone() else { return };
        let (title, kind) = describe(&tool, &input);
        let call_id = r["tool_use_id"].as_str().map_or_else(|| format!("ask-{}", self.next_client_id + 1), String::from);
        let mut call = json!({ "toolCallId": call_id, "title": title, "kind": kind, "status": "pending" });
        let diffs = edit_diffs(&tool, &input);
        if !diffs.is_empty() {
            call["content"] = json!(diffs);
        }
        let for_chat = if tool == "Bash" { "Allow This Command for This Chat" } else { "Allow for This Chat" };
        self.next_client_id += 1;
        let id = self.next_client_id;
        self.asked.insert(id, Asked { request_id, tool, input, rule });
        let options = json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "allow_always", "name": for_chat, "kind": "allow_always" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
        ]);
        self.client(json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": { "sessionId": session, "toolCall": call, "options": options } }));
    }
}

fn is_auth_error(msg: &Value) -> bool {
    let text = msg["result"].as_str().unwrap_or("").to_lowercase();
    msg["terminal_reason"].as_str() == Some("api_error") && (text.contains("authenticate") || text.contains("oauth") || text.contains("log in") || text.contains("login"))
}

/// The editor's MCP servers (ACP's `mcpServers`) as `--mcp-config` JSON.
fn mcp_config(servers: &Value) -> Option<Value> {
    let mut out = serde_json::Map::new();
    for s in servers.as_array()? {
        let (Some(name), Some(command)) = (s["name"].as_str(), s["command"].as_str()) else { continue };
        let env: serde_json::Map<String, Value> = s["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["name"].as_str()?.to_string(), json!(e["value"].as_str()?))))
            .collect();
        out.insert(name.to_string(), json!({ "type": "stdio", "command": command, "args": s["args"].clone(), "env": env }));
    }
    (!out.is_empty()).then(|| json!({ "mcpServers": out }))
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A tool use's title and ACP kind.
fn describe(tool: &str, input: &Value) -> (String, &'static str) {
    let s = |key: &str| input[key].as_str().unwrap_or("").to_string();
    match tool {
        "Bash" => (format!("Run `{}`", s("command").trim()), "execute"),
        "Read" => (format!("Read {}", file_name(&s("file_path"))), "read"),
        "Edit" | "MultiEdit" => (format!("Edit {}", file_name(&s("file_path"))), "edit"),
        "Write" => (format!("Write {}", file_name(&s("file_path"))), "edit"),
        "NotebookEdit" => (format!("Edit {}", file_name(&s("notebook_path"))), "edit"),
        "Glob" => (format!("Find files: {}", s("pattern")), "search"),
        "Grep" => (format!("Search for {}", s("pattern")), "search"),
        "WebFetch" => (format!("Fetch {}", s("url")), "fetch"),
        "WebSearch" => (format!("Search the web: {}", s("query")), "fetch"),
        "Task" | "Agent" => (format!("Subagent: {}", s("description")), "think"),
        _ => match tool.strip_prefix("mcp__").and_then(|t| t.split_once("__")) {
            Some((server, name)) => (format!("{server}: {name}"), "other"),
            None => (tool.to_string(), "other"),
        },
    }
}

/// An edit's diffs (ACP diff content): the file now ↔ after the edit.
fn edit_diffs(tool: &str, input: &Value) -> Vec<Value> {
    let path = input["file_path"].as_str().unwrap_or("");
    if path.is_empty() {
        return Vec::new();
    }
    let old = std::fs::read_to_string(path).ok();
    let edit = |text: &str, e: &Value| -> Option<String> {
        let (from, to) = (e["old_string"].as_str()?, e["new_string"].as_str()?);
        if from.is_empty() || !text.contains(from) {
            return None;
        }
        Some(if e["replace_all"].as_bool() == Some(true) { text.replace(from, to) } else { text.replacen(from, to, 1) })
    };
    let new = match tool {
        "Write" => input["content"].as_str().map(String::from),
        "Edit" => old.as_deref().and_then(|t| edit(t, input)),
        "MultiEdit" => input["edits"].as_array().and_then(|edits| edits.iter().try_fold(old.clone()?, |t, e| edit(&t, e))),
        _ => None,
    };
    let Some(new) = new else { return Vec::new() };
    let mut d = json!({ "type": "diff", "path": path, "newText": new });
    if let Some(old) = old {
        d["oldText"] = json!(old);
    }
    vec![d]
}

fn plan_entries(input: &Value) -> Vec<Value> {
    input["todos"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let status = match t["status"].as_str() {
                Some("completed") => "completed",
                Some("in_progress") => "in_progress",
                _ => "pending",
            };
            json!({ "content": t["content"], "priority": "medium", "status": status })
        })
        .collect()
}

/// A tool result's text (a string, or text blocks).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_tools() {
        assert_eq!(describe("Bash", &json!({ "command": "ls -1 " })), ("Run `ls -1`".into(), "execute"));
        assert_eq!(describe("Edit", &json!({ "file_path": "/a/b.rs" })), ("Edit b.rs".into(), "edit"));
        assert_eq!(describe("mcp__orbvane__diagnostics", &json!({})), ("orbvane: diagnostics".into(), "other"));
        assert_eq!(plan_entries(&json!({ "todos": [{ "content": "A", "status": "in_progress" }] })), vec![json!({ "content": "A", "priority": "medium", "status": "in_progress" })]);
        assert_eq!(result_text(&json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }])), "a\nb");
    }

    #[test]
    fn edits_become_diffs() {
        let dir = std::env::temp_dir().join(format!("orbvane-claude-diffs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one two one\n").unwrap();
        let path = file.to_string_lossy().to_string();
        let d = edit_diffs("Edit", &json!({ "file_path": path, "old_string": "one", "new_string": "1" }));
        assert_eq!((d[0]["oldText"].as_str(), d[0]["newText"].as_str()), (Some("one two one\n"), Some("1 two one\n")));
        let d = edit_diffs("MultiEdit", &json!({ "file_path": path, "edits": [{ "old_string": "one", "new_string": "1", "replace_all": true }, { "old_string": "two", "new_string": "2" }] }));
        assert_eq!(d[0]["newText"].as_str(), Some("1 2 1\n"));
        let d = edit_diffs("Write", &json!({ "file_path": dir.join("new.txt").to_string_lossy(), "content": "x" }));
        assert!(d[0].get("oldText").is_none());
        assert!(edit_diffs("Edit", &json!({ "file_path": path, "old_string": "nine", "new_string": "9" })).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let servers = json!([{ "name": "orbvane", "command": "/bin/o", "args": ["--editor-tools"], "env": [{ "name": "S", "value": "/s" }] }]);
        assert_eq!(mcp_config(&servers), Some(json!({ "mcpServers": { "orbvane": { "type": "stdio", "command": "/bin/o", "args": ["--editor-tools"], "env": { "S": "/s" } } } })));
    }

    #[test]
    fn recognizes_an_expired_sign_in() {
        let msg = json!({ "type": "result", "is_error": true, "terminal_reason": "api_error", "result": "Failed to authenticate: OAuth session expired and could not be refreshed" });
        assert!(is_auth_error(&msg));
        assert!(!is_auth_error(&json!({ "type": "result", "is_error": true, "result": "Overloaded" })));
    }
}
