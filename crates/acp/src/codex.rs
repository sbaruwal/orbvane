//! An Agent Client Protocol agent in front of Codex: runs `codex app-server` (its own JSON-RPC
//! protocol over stdio, one message per line) and translates. Started with
//! `Client::in_process`, so the editor sees an ordinary agent.
//!
//! - `initialize` starts Codex and asks for its models and account; `session/new` is
//!   `thread/start` (with the editor's MCP servers in its config); `session/prompt` is
//!   `turn/start`, answered when `turn/completed` arrives; `session/cancel` is `turn/interrupt`.
//! - Codex's items become session updates: message and reasoning deltas, commands, file changes
//!   (as diffs: the file now ↔ with Codex's patch applied), MCP tool calls, web searches and the
//!   plan.
//! - Its approval requests for commands and file changes become `session/request_permission`.
//!
//! Codex reads and writes files on disk itself (the editor saves before each message and
//! reloads what changed). If the model Codex is configured with isn't one the account offers,
//! the chat uses the account's default and says so.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use serde_json::{json, Value};

use crate::bridge::{Lines, Options};

/// The bridge's errors in our answers.
const INTERNAL: i64 = -32603;
const NOT_FOUND: i64 = -32601;
const AUTH_REQUIRED: i64 = -32000;
/// How many lines of a command's output go in its tool call.
const OUTPUT_LINES: usize = 12;

enum Event {
    /// From the editor.
    Client(Value),
    ClientGone,
    /// From Codex.
    Codex(Value),
    CodexGone,
}

impl From<Option<Value>> for Event {
    fn from(line: Option<Value>) -> Self {
        line.map_or(Event::CodexGone, Event::Codex)
    }
}

/// A request we sent Codex, waiting for its answer.
enum Pending {
    /// Answers the editor's `initialize` with this id.
    Initialize(Value),
    Models,
    Account,
    /// Answers the editor's `session/new`.
    ThreadStart(Value),
    /// The prompt's `turn/start`.
    TurnStart,
    Ignore,
}

/// An approval Codex asked for, now a `session/request_permission` to the editor.
struct Approval {
    codex_id: Value,
}

struct Bridge {
    to_client: Sender<Value>,
    log: Sender<String>,
    events: Sender<Event>,
    options: Options,
    codex: Option<Lines>,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    /// Our requests to the editor (permission questions), by id.
    asked: HashMap<i64, Approval>,
    next_client_id: i64,
    version: String,
    signed_in: Option<bool>,
    /// (id, display name) of the account's models, and the default's id.
    models: Vec<(String, String)>,
    default_model: Option<String>,
    /// The model each turn asks for, when the configured one isn't offered.
    model_override: Option<String>,
    thread: Option<String>,
    turn: Option<String>,
    /// The editor's `session/prompt` waiting for the turn to end.
    prompt: Option<Value>,
    /// Message items that streamed deltas (their completion adds nothing), and the last
    /// message item written (a new one starts a new paragraph).
    streamed: HashSet<String>,
    last_message: Option<String>,
    /// File change items' diffs (ACP diff content), for their permission questions.
    diffs: HashMap<String, Vec<Value>>,
}

/// Runs the bridge until the editor closes its end. See `Client::in_process`.
pub fn serve(options: Options, rx: Receiver<Value>, tx: Sender<Value>, log: Sender<String>) {
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
        codex: None,
        next_id: 0,
        pending: HashMap::new(),
        asked: HashMap::new(),
        next_client_id: 0,
        version: String::new(),
        signed_in: None,
        models: Vec::new(),
        default_model: None,
        model_override: None,
        thread: None,
        turn: None,
        prompt: None,
        streamed: HashSet::new(),
        last_message: None,
        diffs: HashMap::new(),
    };
    for event in inbox {
        match event {
            Event::Client(msg) => b.from_client(msg),
            Event::Codex(msg) => b.from_codex(msg),
            Event::ClientGone => break,
            Event::CodexGone => {
                let _ = b.log.send("Codex exited.".into());
                break;
            }
        }
    }
    if let Some(c) = b.codex.take() {
        c.end();
    }
}

impl Bridge {
    // ------------------------------------------------------------------ sending

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
        let Some(thread) = &self.thread else { return };
        self.client(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": thread, "update": update } }));
    }

    fn codex_send(&mut self, msg: Value) {
        let Some(c) = &mut self.codex else { return };
        if !c.send(&msg) {
            let _ = self.events.send(Event::CodexGone);
        }
    }

    fn codex_request(&mut self, method: &str, params: Value, pending: Pending) {
        self.next_id += 1;
        let id = self.next_id;
        self.pending.insert(id, pending);
        self.codex_send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn start_codex(&mut self) -> std::io::Result<()> {
        self.codex = Some(Lines::spawn(&self.options, self.events.clone(), self.log.clone())?);
        Ok(())
    }

    // ------------------------------------------------------------------ the editor

    fn from_client(&mut self, msg: Value) {
        let method = msg["method"].as_str().map(str::to_string);
        let id = msg.get("id").cloned();
        match (method.as_deref(), id) {
            (Some(method), Some(id)) => self.client_request(method, id, &msg["params"]),
            (Some(method), None) => self.client_notification(method, &msg["params"]),
            (None, Some(id)) => self.client_answer(id, &msg),
            (None, None) => {}
        }
    }

    fn client_request(&mut self, method: &str, id: Value, params: &Value) {
        match method {
            "initialize" => {
                if self.codex.is_none() {
                    if let Err(e) = self.start_codex() {
                        return self.reply_error(id, INTERNAL, &format!("Couldn't start Codex: {e}"));
                    }
                }
                let version = params["clientInfo"]["version"].as_str().unwrap_or("").to_string();
                let info = json!({ "clientInfo": { "name": "orbvane", "title": "Orbvane", "version": version } });
                self.codex_request("initialize", info, Pending::Initialize(id));
            }
            "session/new" => {
                if self.signed_in == Some(false) {
                    return self.reply_error(id, AUTH_REQUIRED, "Codex isn't signed in. Sign in to Codex from the agent menu, then start a new chat.");
                }
                let cwd = params["cwd"].as_str().map_or_else(|| self.options.cwd.clone(), PathBuf::from);
                let mut config = json!({});
                if let Some(servers) = mcp_servers(&params["mcpServers"]) {
                    config["mcp_servers"] = servers;
                }
                self.thread = None;
                let start = json!({ "cwd": cwd, "approvalPolicy": "untrusted", "sandbox": "workspace-write", "config": config });
                self.codex_request("thread/start", start, Pending::ThreadStart(id));
            }
            "session/prompt" => {
                let (Some(thread), None) = (self.thread.clone(), &self.prompt) else {
                    return self.reply_error(id, INTERNAL, "Codex is still answering the last message.");
                };
                let text = prompt_text(&params["prompt"]);
                let mut turn = json!({ "threadId": thread, "input": [{ "type": "text", "text": text, "text_elements": [] }] });
                if let Some(model) = &self.model_override {
                    turn["model"] = json!(model);
                }
                self.prompt = Some(id);
                self.last_message = None;
                self.codex_request("turn/start", turn, Pending::TurnStart);
            }
            "authenticate" => self.reply_error(id, INTERNAL, "Sign in to Codex from the agent menu (it runs `codex login`)."),
            _ => self.reply_error(id, NOT_FOUND, &format!("{method} isn't supported")),
        }
    }

    fn client_notification(&mut self, method: &str, _params: &Value) {
        if method == "session/cancel" {
            if let (Some(thread), Some(turn)) = (self.thread.clone(), self.turn.clone()) {
                self.codex_request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }), Pending::Ignore);
            }
        }
    }

    /// The editor answered a permission question: Codex gets its decision.
    fn client_answer(&mut self, id: Value, msg: &Value) {
        let Some(approval) = id.as_i64().and_then(|id| self.asked.remove(&id)) else { return };
        let outcome = &msg["result"]["outcome"];
        let decision = match (outcome["outcome"].as_str(), outcome["optionId"].as_str()) {
            (Some("selected"), Some(option @ ("accept" | "acceptForSession" | "decline"))) => option,
            (Some("cancelled"), _) => "cancel",
            _ => "decline",
        };
        self.codex_send(json!({ "jsonrpc": "2.0", "id": approval.codex_id, "result": { "decision": decision } }));
    }

    // ------------------------------------------------------------------ Codex

    fn from_codex(&mut self, msg: Value) {
        let method = msg["method"].as_str().map(str::to_string);
        match (method, msg.get("id").cloned()) {
            (Some(method), Some(id)) => self.codex_request_in(&method, id, &msg["params"]),
            (Some(method), None) => self.codex_notification(&method, &msg["params"]),
            (None, Some(id)) => {
                let Some(pending) = id.as_i64().and_then(|id| self.pending.remove(&id)) else { return };
                let result = match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("Codex reported an error").to_string()),
                    None => Ok(msg["result"].clone()),
                };
                self.codex_answer(pending, result);
            }
            (None, None) => {}
        }
    }

    fn codex_answer(&mut self, pending: Pending, result: Result<Value, String>) {
        match (pending, result) {
            (Pending::Initialize(id), Ok(r)) => {
                self.codex_send(json!({ "jsonrpc": "2.0", "method": "initialized" }));
                self.codex_request("model/list", json!({}), Pending::Models);
                self.codex_request("account/read", json!({}), Pending::Account);
                // "orbvane/0.156.1 (Mac OS ...)": Codex's version.
                let agent = r["userAgent"].as_str().unwrap_or("");
                self.version = agent.split_whitespace().next().and_then(|s| s.split('/').nth(1)).unwrap_or("").to_string();
                self.reply(
                    id,
                    json!({
                        "protocolVersion": crate::PROTOCOL_VERSION,
                        "agentCapabilities": { "loadSession": false, "promptCapabilities": { "embeddedContext": true, "image": false, "audio": false } },
                        "agentInfo": { "name": "codex", "title": "Codex", "version": self.version },
                        "authMethods": [],
                    }),
                );
            }
            (Pending::Initialize(id), Err(e)) => self.reply_error(id, INTERNAL, &format!("Codex didn't start: {e}")),
            (Pending::Models, Ok(r)) => {
                for m in r["data"].as_array().into_iter().flatten() {
                    let (Some(id), name) = (m["id"].as_str(), m["displayName"].as_str()) else { continue };
                    self.models.push((id.to_string(), name.unwrap_or(id).to_string()));
                    if m["isDefault"].as_bool() == Some(true) {
                        self.default_model = Some(id.to_string());
                    }
                }
            }
            (Pending::Account, Ok(r)) => {
                let needs = r["requiresOpenaiAuth"].as_bool().unwrap_or(false);
                self.signed_in = Some(!needs || !r["account"].is_null());
            }
            (Pending::ThreadStart(id), Ok(r)) => {
                let Some(thread) = r["thread"]["id"].as_str() else {
                    return self.reply_error(id, INTERNAL, "Codex didn't start a conversation.");
                };
                self.thread = Some(thread.to_string());
                self.reply(id, json!({ "sessionId": thread }));
                self.check_model(r["model"].as_str().unwrap_or(""));
            }
            (Pending::ThreadStart(id), Err(e)) => self.reply_error(id, INTERNAL, &friendly(&e)),
            (Pending::TurnStart, Ok(r)) => self.turn = r["turn"]["id"].as_str().map(String::from),
            (Pending::TurnStart, Err(e)) => {
                if let Some(id) = self.prompt.take() {
                    self.reply_error(id, INTERNAL, &friendly(&e));
                }
            }
            (_, Err(e)) => {
                let _ = self.log.send(format!("Codex: {e}"));
            }
            _ => {}
        }
    }

    /// The thread's model isn't one the account offers (an old setting in Codex's config):
    /// turns use the default one instead, and the chat says so.
    fn check_model(&mut self, model: &str) {
        self.model_override = None;
        if model.is_empty() || self.models.is_empty() || self.models.iter().any(|(id, _)| id == model) {
            return;
        }
        let Some(default) = self.default_model.clone() else { return };
        let name = self.models.iter().find(|(id, _)| *id == default).map_or(default.clone(), |(_, n)| n.clone());
        self.model_override = Some(default);
        let text = format!("Codex is set to use the model {model}, which this account doesn't offer, so this chat uses {name}. (Codex's model is set in ~/.codex/config.toml.)");
        self.update(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text } }));
    }

    fn codex_notification(&mut self, method: &str, p: &Value) {
        match method {
            "item/agentMessage/delta" => {
                let item = p["itemId"].as_str().unwrap_or("").to_string();
                let mut text = p["delta"].as_str().unwrap_or("").to_string();
                if self.last_message.as_deref() != Some(item.as_str()) {
                    if self.last_message.is_some() {
                        text.insert_str(0, "\n\n");
                    }
                    self.last_message = Some(item.clone());
                }
                self.streamed.insert(item);
                self.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }));
            }
            "item/reasoning/summaryTextDelta" => {
                let text = p["delta"].as_str().unwrap_or("");
                self.update(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text } }));
            }
            "item/started" => self.item_started(&p["item"]),
            "item/completed" => self.item_completed(&p["item"]),
            "item/fileChange/patchUpdated" => {
                let id = p["itemId"].as_str().unwrap_or("").to_string();
                let diffs = file_diffs(&p["changes"]);
                self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "content": diffs }));
                self.diffs.insert(id, diffs);
            }
            "turn/plan/updated" => {
                let entries: Vec<Value> = p["plan"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        let status = match s["status"].as_str() {
                            Some("completed") => "completed",
                            Some("inProgress") => "in_progress",
                            _ => "pending",
                        };
                        json!({ "content": s["step"], "priority": "medium", "status": status })
                    })
                    .collect();
                self.update(json!({ "sessionUpdate": "plan", "entries": entries }));
            }
            "turn/started" => self.turn = p["turn"]["id"].as_str().map(String::from),
            "turn/completed" => self.turn_completed(&p["turn"]),
            "error" => {
                let message = friendly(p["error"]["message"].as_str().unwrap_or(""));
                let retry = if p["willRetry"].as_bool() == Some(true) { " (retrying)" } else { "" };
                let _ = self.log.send(format!("Codex: {message}{retry}"));
            }
            "warning" | "configWarning" | "deprecationNotice" | "guardianWarning" => {
                let message = p["message"].as_str().or(p["summary"].as_str()).unwrap_or("");
                let _ = self.log.send(format!("Codex: {message}"));
            }
            "account/updated" => {
                if p["authMode"].is_string() {
                    self.signed_in = Some(true);
                }
            }
            _ => {}
        }
    }

    fn turn_completed(&mut self, turn: &Value) {
        self.turn = None;
        self.streamed.clear();
        let Some(id) = self.prompt.take() else { return };
        match turn["status"].as_str() {
            Some("interrupted") => self.reply(id, json!({ "stopReason": "cancelled" })),
            Some("failed") => {
                let message = turn["error"]["message"].as_str().unwrap_or("The turn failed.");
                self.reply_error(id, INTERNAL, &friendly(message));
            }
            _ => self.reply(id, json!({ "stopReason": "end_turn" })),
        }
    }

    fn item_started(&mut self, item: &Value) {
        let id = item["id"].as_str().unwrap_or("").to_string();
        let call = match item["type"].as_str() {
            Some("commandExecution") => json!({ "title": command_title(item["command"].as_str().unwrap_or("")), "kind": "execute" }),
            Some("fileChange") => {
                let diffs = file_diffs(&item["changes"]);
                self.diffs.insert(id.clone(), diffs.clone());
                json!({ "title": edit_title(&item["changes"]), "kind": "edit", "content": diffs })
            }
            Some("mcpToolCall") => json!({ "title": format!("{}: {}", item["server"].as_str().unwrap_or(""), item["tool"].as_str().unwrap_or("")), "kind": "other" }),
            Some("webSearch") => json!({ "title": format!("Search the web: {}", item["query"].as_str().unwrap_or("")), "kind": "fetch" }),
            Some("dynamicToolCall") => json!({ "title": item["tool"], "kind": "other" }),
            _ => return,
        };
        let mut update = json!({ "sessionUpdate": "tool_call", "toolCallId": id, "status": "in_progress" });
        merge(&mut update, call);
        self.update(update);
    }

    fn item_completed(&mut self, item: &Value) {
        let id = item["id"].as_str().unwrap_or("").to_string();
        match item["type"].as_str() {
            Some("agentMessage") if !self.streamed.contains(&id) => {
                let mut text = item["text"].as_str().unwrap_or("").to_string();
                if text.is_empty() {
                    return;
                }
                if self.last_message.is_some() {
                    text.insert_str(0, "\n\n");
                }
                self.last_message = Some(id);
                self.update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }));
            }
            Some("commandExecution") => {
                let failed = matches!(item["status"].as_str(), Some("failed" | "declined")) || item["exitCode"].as_i64().is_some_and(|c| c != 0);
                let output = tail(item["aggregatedOutput"].as_str().unwrap_or(""), OUTPUT_LINES);
                let mut update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": if failed { "failed" } else { "completed" } });
                if !output.trim().is_empty() {
                    update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": output } }]);
                }
                self.update(update);
            }
            Some("fileChange" | "mcpToolCall" | "webSearch" | "dynamicToolCall") => {
                let failed = matches!(item["status"].as_str(), Some("failed" | "declined"));
                self.update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": if failed { "failed" } else { "completed" } }));
            }
            _ => {}
        }
    }

    /// Codex asks us something: approvals go to the editor, the rest is declined.
    fn codex_request_in(&mut self, method: &str, codex_id: Value, p: &Value) {
        let item = p["itemId"].as_str().unwrap_or("").to_string();
        let (tool_call, allow_session) = match method {
            "item/commandExecution/requestApproval" => {
                let command = p["command"].as_str().unwrap_or("");
                let mut call = json!({ "toolCallId": item, "title": command_title(command), "kind": "execute", "status": "pending" });
                if let Some(reason) = p["reason"].as_str().filter(|r| !r.is_empty()) {
                    call["content"] = json!([{ "type": "content", "content": { "type": "text", "text": reason } }]);
                }
                (call, "Allow for This Chat")
            }
            "item/fileChange/requestApproval" => {
                let diffs = self.diffs.get(&item).cloned().unwrap_or_default();
                let title = if diffs.is_empty() { "Change files".to_string() } else { edit_title_from_diffs(&diffs) };
                (json!({ "toolCallId": item, "title": title, "kind": "edit", "status": "pending", "content": diffs }), "Allow These Files for This Chat")
            }
            _ => {
                let _ = self.log.send(format!("Codex asked for {method}, which Orbvane doesn't support; declined."));
                return self.codex_send(json!({ "jsonrpc": "2.0", "id": codex_id, "error": { "code": NOT_FOUND, "message": format!("{method} isn't supported") } }));
            }
        };
        let Some(thread) = self.thread.clone() else { return };
        self.next_client_id += 1;
        let id = self.next_client_id;
        self.asked.insert(id, Approval { codex_id });
        let options = json!([
            { "optionId": "accept", "name": "Allow", "kind": "allow_once" },
            { "optionId": "acceptForSession", "name": allow_session, "kind": "allow_always" },
            { "optionId": "decline", "name": "Reject", "kind": "reject_once" },
        ]);
        self.client(json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": { "sessionId": thread, "toolCall": tool_call, "options": options } }));
    }
}

/// Adds `extra`'s fields to `v`.
fn merge(v: &mut Value, extra: Value) {
    if let (Some(v), Value::Object(extra)) = (v.as_object_mut(), extra) {
        v.extend(extra);
    }
}

/// The editor's MCP servers (ACP's `mcpServers`: name, command, args, env as name/value pairs)
/// as Codex's `mcp_servers` config.
fn mcp_servers(servers: &Value) -> Option<Value> {
    let mut out = serde_json::Map::new();
    for s in servers.as_array()? {
        let (Some(name), Some(command)) = (s["name"].as_str(), s["command"].as_str()) else { continue };
        let env: serde_json::Map<String, Value> = s["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["name"].as_str()?.to_string(), json!(e["value"].as_str()?))))
            .collect();
        out.insert(name.to_string(), json!({ "command": command, "args": s["args"].clone(), "env": env }));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// The prompt's content blocks as one text: Codex takes text (its file inputs are images).
pub(crate) fn prompt_text(blocks: &Value) -> String {
    let mut parts = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or("").to_string()),
            Some("resource_link") => {
                let path = uri_path(b["uri"].as_str().unwrap_or(""));
                parts.push(format!("(The file open in the editor: {path})"));
            }
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                if let Some(text) = r["text"].as_str() {
                    parts.push(format!("{}:\n```\n{text}\n```", uri_path(uri)));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// `file:///a/b%20c.rs#L1-2` → `/a/b c.rs#L1-2`.
fn uri_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn command_title(command: &str) -> String {
    format!("Run `{}`", command.trim())
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

fn edit_title(changes: &Value) -> String {
    let names: Vec<String> = changes.as_array().into_iter().flatten().filter_map(|c| c["path"].as_str()).map(file_name).collect();
    titled(&names)
}

fn edit_title_from_diffs(diffs: &[Value]) -> String {
    let names: Vec<String> = diffs.iter().filter_map(|d| d["path"].as_str()).map(file_name).collect();
    titled(&names)
}

fn titled(names: &[String]) -> String {
    match names {
        [] => "Change files".into(),
        [one] => format!("Edit {one}"),
        [first, rest @ ..] => format!("Edit {first} and {} more", rest.len()),
    }
}

/// A file change's diffs as ACP diff content: the file as it is now, and as the change leaves
/// it (Codex sends unified diffs).
fn file_diffs(changes: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for c in changes.as_array().into_iter().flatten() {
        let Some(path) = c["path"].as_str() else { continue };
        let diff = c["diff"].as_str().unwrap_or("");
        let old = std::fs::read_to_string(path).ok();
        let new = match c["kind"]["type"].as_str() {
            Some("delete") => Some(String::new()),
            Some("add") if !diff.lines().any(|l| l.starts_with("@@")) => Some(diff.to_string()),
            _ => apply_unified(old.as_deref().unwrap_or(""), diff),
        };
        let Some(new) = new else { continue };
        let target = c["kind"]["move_path"].as_str().unwrap_or(path);
        let mut d = json!({ "type": "diff", "path": target, "newText": new });
        if c["kind"]["type"].as_str() != Some("add") {
            d["oldText"] = json!(old.unwrap_or_default());
        }
        out.push(d);
    }
    out
}

/// Applies a unified diff to `old`. Each hunk is found by its lines (context and removed),
/// at the position its header names when that matches, else searching forward; None when a
/// hunk can't be placed.
pub fn apply_unified(old: &str, diff: &str) -> Option<String> {
    let lines: Vec<&str> = old.split_inclusive('\n').collect();
    let trim = |l: &str| l.strip_suffix('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).unwrap_or(l).to_string();
    // (header's old start, lines before, lines after)
    let mut hunks: Vec<(Option<usize>, Vec<String>, Vec<String>)> = Vec::new();
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@") {
            let start = header.trim().strip_prefix('-').and_then(|s| s.split([',', ' ']).next()).and_then(|n| n.parse::<usize>().ok());
            hunks.push((start, Vec::new(), Vec::new()));
            continue;
        }
        let Some((_, before, after)) = hunks.last_mut() else { continue };
        if line.starts_with("\\ ") {
            continue;
        }
        match line.chars().next() {
            Some('-') => before.push(line[1..].to_string()),
            Some('+') => after.push(line[1..].to_string()),
            Some(' ') => {
                before.push(line[1..].to_string());
                after.push(line[1..].to_string());
            }
            None => {
                before.push(String::new());
                after.push(String::new());
            }
            _ => {}
        }
    }
    if hunks.is_empty() {
        return None;
    }
    let ends_with_newline = old.is_empty() || old.ends_with('\n');
    let old_lines: Vec<String> = lines.iter().map(|l| trim(l)).collect();
    let mut out: Vec<String> = Vec::new();
    let mut at = 0;
    for (start, before, after) in hunks {
        let fits = |i: usize| i + before.len() <= old_lines.len() && old_lines[i..i + before.len()] == before[..];
        let hinted = start.map(|s| s.saturating_sub(1)).filter(|&i| i >= at && fits(i));
        let found = hinted.or_else(|| (at..=old_lines.len()).find(|&i| fits(i)))?;
        out.extend(old_lines[at..found].iter().cloned());
        out.extend(after);
        at = found + before.len();
    }
    out.extend(old_lines[at..].iter().cloned());
    let mut text = out.join("\n");
    if !text.is_empty() && ends_with_newline {
        text.push('\n');
    }
    Some(text)
}

/// The last `n` lines of `text`.
pub(crate) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if start > 0 {
        out.insert_str(0, "…\n");
    }
    out
}

/// Codex's error messages are sometimes the API's error as JSON: its inner message.
fn friendly(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).map(String::from))
        .unwrap_or_else(|| message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_unified_diffs() {
        let old = "one\ntwo\nthree\nfour\n";
        let diff = "--- a/notes.txt\n+++ b/notes.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        assert_eq!(apply_unified(old, diff).as_deref(), Some("one\nTWO\nthree\nfour\n"));
        // A wrong line number: found by its lines.
        let diff = "@@ -9,2 +9,2 @@\n three\n-four\n+4\n";
        assert_eq!(apply_unified(old, diff).as_deref(), Some("one\ntwo\nthree\n4\n"));
        // Two hunks, and an addition at the end.
        let diff = "@@ -1,1 +1,1 @@\n-one\n+1\n@@ -4,1 +4,2 @@\n four\n+five\n";
        assert_eq!(apply_unified(old, diff).as_deref(), Some("1\ntwo\nthree\nfour\nfive\n"));
        // A hunk that doesn't fit.
        assert_eq!(apply_unified(old, "@@ -1 +1 @@\n-nine\n+9\n"), None);
        // A new file.
        assert_eq!(apply_unified("", "@@ -0,0 +1,2 @@\n+a\n+b\n").as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn translates_inputs() {
        let prompt = json!([
            { "type": "text", "text": "fix it" },
            { "type": "resource", "resource": { "uri": "file:///p/a%20b.rs#L2-3", "text": "let x = 1;" } },
            { "type": "resource_link", "uri": "file:///p/a%20b.rs", "name": "a b.rs" },
        ]);
        assert_eq!(prompt_text(&prompt), "fix it\n\n/p/a b.rs#L2-3:\n```\nlet x = 1;\n```\n\n(The file open in the editor: /p/a b.rs)");
        let servers = json!([{ "name": "orbvane", "command": "/bin/o", "args": ["--editor-tools"], "env": [{ "name": "S", "value": "/tmp/s" }] }]);
        assert_eq!(mcp_servers(&servers), Some(json!({ "orbvane": { "command": "/bin/o", "args": ["--editor-tools"], "env": { "S": "/tmp/s" } } })));
        assert_eq!(mcp_servers(&json!([])), None);
        assert_eq!(friendly(r#"{"type":"error","status":400,"error":{"message":"No such model."}}"#), "No such model.");
        assert_eq!(friendly("plain"), "plain");
        assert_eq!(tail("a\nb\nc", 2), "…\nb\nc");
        assert_eq!(edit_title(&json!([{ "path": "/x/a.rs" }, { "path": "/x/b.rs" }])), "Edit a.rs and 1 more");
    }
}
