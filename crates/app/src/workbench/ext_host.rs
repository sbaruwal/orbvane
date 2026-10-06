//! The extension host: runs the enabled extensions' programs (one process each, JSON-RPC over
//! stdio; the protocol is documented in the `orbvane-extension` crate) when their activation
//! events happen, answers what they ask (messages, quick picks, input boxes, the status bar,
//! output channels, documents, edits, settings, commands) and tells them what happens
//! (documents opened, changed, saved and closed, the active editor and its selection, settings).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use extensions::Extension;
use lsp::{Encoding, Incoming};
use serde_json::{json, Value};
use text::{Pos, Selection};

use super::notifications::Severity;
use super::Workbench;
use crate::commands::{self, Command};
use crate::palette::{Action, InputBox, Item, Palette, Picker};

const INITIALIZE_ID: i64 = 0;

struct Proc {
    ext: Extension,
    conn: lsp::Connection,
    /// `initialize` was answered; messages wait in `queued` until then.
    ready: bool,
    queued: Vec<Value>,
    next_id: i64,
    /// Asked to stop (an exit then isn't a crash).
    stopping: bool,
}

/// Who waits for the answer to a request the editor sent an extension.
pub(super) enum Waiter {
    Nobody,
    /// Another extension's `commands/execute`: (extension id, its request id).
    Extension(String, Value),
    /// `treeView/getChildren` for (view, element).
    Tree(String, String),
    /// A hover provider, for (document, position).
    Hover(usize, Pos),
    /// A completion provider, for completion request `seq`.
    Completion(u64),
    Definition,
    /// Code actions from extension `ext`: for Quick Fix, or the lightbulb's request `seq`.
    CodeActions { ext: String, auto: Option<u64> },
    /// Formatting edits for the document at `path` at buffer `version` (`save`: format on save).
    Formatting { path: std::path::PathBuf, version: u64, save: bool },
}

/// An extension's status bar entry.
#[derive(Clone, Debug)]
pub(super) struct StatusItem {
    pub ext: String,
    pub id: String,
    pub text: String,
    pub command: Option<String>,
    pub right: bool,
    pub priority: i64,
}

/// A quick pick or input box an extension is waiting on.
struct Question {
    ext: String,
    request: Value,
}

/// The state documents were in when last reported to the extensions.
#[derive(Clone, PartialEq)]
struct DocState {
    path: Option<PathBuf>,
    version: u64,
}

#[derive(Default)]
pub(super) struct ExtHost {
    procs: Vec<Proc>,
    /// `*` and `onStartupFinished` have fired.
    started: bool,
    /// Languages whose `onLanguage` event has fired.
    languages: HashSet<String>,
    waiting: HashMap<(String, i64), Waiter>,
    question: Option<Question>,
    /// Quick picks and input boxes waiting their turn: (extension, request id, method, params).
    questions: Vec<(String, Value, String, Value)>,
    pub status: Vec<StatusItem>,
    docs: HashMap<usize, DocState>,
    /// (document, selections) of the active editor when last reported.
    active: Option<(usize, Vec<Selection>)>,
}

/// A document as the protocol describes it (with its text if `text`).
pub(super) fn doc_json(doc: &crate::editor::Doc, text: bool) -> Value {
    let mut v = json!({
        "path": doc.buffer.path(),
        "name": doc.title(),
        "languageId": doc.lang.id(),
        "version": doc.buffer.version(),
        "isDirty": doc.buffer.is_dirty(),
    });
    if text {
        v["text"] = doc.buffer.text().into();
    }
    v
}

/// A selection as a protocol range (UTF-8 byte columns).
fn range_json(doc: &crate::editor::Doc, s: &Selection) -> Value {
    let (a, z) = s.ordered();
    let pos = |p: Pos| json!({ "line": p.line, "character": Encoding::Utf8.to_lsp(&doc.buffer.line(p.line), p.col) });
    json!({ "start": pos(a), "end": pos(z) })
}

/// Whether a `workspaceContains` pattern (`**/Cargo.toml`, `package.json`) matches in `folder`.
fn folder_contains(folder: &Path, pattern: &str) -> bool {
    match pattern.strip_prefix("**/") {
        None => folder.join(pattern).exists(),
        Some(name) => {
            // A shallow walk, giving up after a while, skipping heavy folders.
            fn walk(dir: &Path, name: &str, depth: usize) -> bool {
                if dir.join(name).exists() {
                    return true;
                }
                if depth == 0 {
                    return false;
                }
                let Ok(entries) = std::fs::read_dir(dir) else { return false };
                entries.flatten().any(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    e.file_type().is_ok_and(|t| t.is_dir()) && !n.starts_with('.') && !matches!(n.as_ref(), "node_modules" | "target" | "build" | "dist") && walk(&e.path(), name, depth - 1)
                })
            }
            walk(folder, name, 4)
        }
    }
}

impl Workbench {
    fn ext_proc(&mut self, ext: &str) -> Option<&mut Proc> {
        self.ext_host.procs.iter_mut().find(|p| p.ext.id == ext)
    }

    fn ext_send(&mut self, ext: &str, msg: Value) {
        let Some(p) = self.ext_proc(ext) else { return };
        if p.ready {
            p.conn.send(msg);
        } else {
            p.queued.push(msg);
        }
    }

    fn ext_request(&mut self, ext: &str, method: &str, params: Value, waiter: Waiter) {
        let Some(p) = self.ext_proc(ext) else { return };
        let id = p.next_id;
        p.next_id += 1;
        self.ext_send(ext, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.ext_host.waiting.insert((ext.to_string(), id), waiter);
    }

    /// Sends a request whose answer goes to `waiter` (views, language features).
    pub(super) fn ext_ask(&mut self, ext: &str, method: &str, params: Value, waiter: Waiter) {
        self.ext_request(ext, method, params, waiter);
    }

    fn ext_notify(&mut self, ext: &str, method: &str, params: Value) {
        self.ext_send(ext, json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Tells every running extension.
    fn ext_broadcast(&mut self, method: &str, params: Value) {
        let ids: Vec<String> = self.ext_host.procs.iter().map(|p| p.ext.id.clone()).collect();
        for id in ids {
            self.ext_notify(&id, method, params.clone());
        }
    }

    /// Answers a request an extension sent.
    pub(super) fn ext_respond(&mut self, ext: &str, id: Value, result: Result<Value, String>) {
        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e } }),
        };
        self.ext_send(ext, msg);
    }

    /// Starts the extensions waiting for `event` (`onCommand:x`, `onLanguage:rust`...).
    pub(super) fn ext_activate(&mut self, event: &str) {
        for e in crate::contributions::enabled() {
            if e.program().is_some() && e.activation_events().iter().any(|a| a == event || a == "*") {
                self.ext_start(&e, event);
            }
        }
    }

    /// Runs an extension's program, if it isn't running.
    pub(super) fn ext_start(&mut self, e: &Extension, event: &str) {
        if self.ext_host.procs.iter().any(|p| p.ext.id == e.id) {
            return;
        }
        let Some(program) = e.program() else { return };
        let conn = match lsp::Connection::spawn(&e.id, &program, &[], &e.path, &[("ORBVANE_EXTENSION".into(), e.id.clone())], self.waker.clone()) {
            Ok(c) => c,
            Err(err) => {
                let msg = format!("Activating extension '{}' failed: {}: {err}", e.id, program.display());
                self.output.append(&e.display_name, &format!("{msg}\n"));
                return self.notify(Severity::Error, &msg, &e.display_name, Vec::new(), None);
            }
        };
        let folders: Vec<PathBuf> = self.folders();
        conn.request(
            INITIALIZE_ID,
            "initialize",
            json!({
                "editor": { "name": "orbvane", "version": env!("CARGO_PKG_VERSION") },
                "extension": { "id": e.id, "path": e.path, "version": e.version },
                "workspaceFolders": folders,
                "activationEvent": event,
            }),
        );
        self.ext_host.procs.push(Proc { ext: e.clone(), conn, ready: false, queued: Vec::new(), next_id: INITIALIZE_ID + 1, stopping: false });
    }

    /// Stops an extension's program (it was disabled or uninstalled, or the editor quits).
    pub(super) fn ext_stop(&mut self, ext: &str) {
        let Some(i) = self.ext_host.procs.iter().position(|p| p.ext.id == ext) else { return };
        let mut p = self.ext_host.procs.remove(i);
        p.stopping = true;
        if p.ready {
            p.conn.request(p.next_id, "shutdown", Value::Null);
            p.conn.notify("exit", Value::Null);
        }
        p.conn.stop();
        self.ext_forget(ext);
    }

    /// Drops what an extension left behind: its status bar items, waiters and questions.
    fn ext_forget(&mut self, ext: &str) {
        self.ext_host.status.retain(|s| s.ext != ext);
        self.ext_forget_decorations(ext);
        self.ext_forget_languages(ext);
        let waiting: Vec<((String, i64), Waiter)> = self.ext_host.waiting.extract_if(|(e, _), _| e == ext).collect();
        for (_, waiter) in waiting {
            if let Waiter::Extension(other, id) = waiter {
                self.ext_respond(&other, id, Err(format!("extension '{ext}' stopped")));
            }
        }
        self.ext_host.questions.retain(|q| q.0 != ext);
    }

    pub(super) fn ext_shutdown(&mut self) {
        let ids: Vec<String> = self.ext_host.procs.iter().map(|p| p.ext.id.clone()).collect();
        for id in ids {
            self.ext_stop(&id);
        }
    }

    /// Runs a command by id: the editor's, or an extension's (starting it). The result goes to
    /// `reply_to` (an extension's request) if given.
    pub(super) fn ext_execute(&mut self, command: &str, args: Vec<Value>, reply_to: Option<(String, Value)>) {
        let owner = Command::from_id(command).and_then(commands::ext_command_owner);
        let builtin = Command::from_id(command).filter(|c| !matches!(c, Command::Ext(_)));
        let reply = |wb: &mut Workbench, result: Result<Value, String>| {
            if let Some((ext, id)) = &reply_to {
                wb.ext_respond(ext, id.clone(), result);
            }
        };
        if let Some(cmd) = builtin {
            self.run(cmd);
            return reply(self, Ok(Value::Null));
        }
        // The commands that take arguments.
        match command {
            "setContext" => {
                let key = args.first().and_then(Value::as_str).unwrap_or("").to_string();
                let value = match args.get(1) {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Null) | None => String::new(),
                    Some(v) => v.to_string(),
                };
                self.ext_views.context.insert(key, value);
                return reply(self, Ok(Value::Null));
            }
            "orbvane.open" => {
                let target = args.first().and_then(Value::as_str).unwrap_or("");
                let path = lsp::uri_to_path(target).unwrap_or_else(|| PathBuf::from(target));
                if target.starts_with("http://") || target.starts_with("https://") {
                    let _ = std::process::Command::new("open").arg(target).spawn();
                } else {
                    self.open_file(&path);
                }
                return reply(self, Ok(Value::Null));
            }
            _ => {}
        }
        let Some(owner) = owner.and_then(|o| crate::contributions::with(|r| r.get(&o).cloned())) else {
            let msg = format!("command '{command}' not found");
            if reply_to.is_none() {
                self.notify(Severity::Error, &msg, "", Vec::new(), None);
            }
            return reply(self, Err(msg));
        };
        if owner.program().is_none() {
            let msg = if owner.has_js_code() {
                format!("Command '{command}' can't run: the code of extension '{}' is JavaScript, which Orbvane doesn't run.", owner.display_name)
            } else {
                format!("command '{command}' not found")
            };
            if reply_to.is_none() {
                self.notify(Severity::Warning, &msg, &owner.display_name, Vec::new(), None);
            }
            return reply(self, Err(msg));
        }
        self.ext_start(&owner, &format!("onCommand:{command}"));
        let waiter = match reply_to {
            Some((ext, id)) => Waiter::Extension(ext, id),
            None => Waiter::Nobody,
        };
        self.ext_request(&owner.id, "executeCommand", json!({ "command": command, "arguments": args }), waiter);
    }

    /// Each frame: activation events, the extensions' messages, questions and document events.
    pub(super) fn ext_tick(&mut self) {
        if !self.ext_host.started {
            self.ext_host.started = true;
            self.ext_activate("onStartupFinished");
            self.ext_workspace_contains();
        }
        let langs: Vec<String> = self.docs.iter().flatten().map(|d| d.lang.id().to_string()).collect();
        for lang in langs {
            if self.ext_host.languages.insert(lang.clone()) {
                self.ext_activate(&format!("onLanguage:{lang}"));
            }
        }
        for i in (0..self.ext_host.procs.len()).rev() {
            let incoming = self.ext_host.procs[i].conn.poll();
            let ext = self.ext_host.procs[i].ext.clone();
            for msg in incoming {
                self.ext_incoming(&ext, msg);
            }
        }
        self.ext_questions_tick();
        self.ext_documents_tick();
    }

    fn ext_incoming(&mut self, ext: &Extension, msg: Incoming) {
        match msg {
            Incoming::Response { id: INITIALIZE_ID, result } => match result {
                Ok(_) => self.ext_ready(&ext.id),
                Err(e) => self.output.append(&ext.display_name, &format!("initialize failed: {e}\n")),
            },
            Incoming::Response { id, result } => match self.ext_host.waiting.remove(&(ext.id.clone(), id)) {
                Some(Waiter::Extension(other, request)) => self.ext_respond(&other, request, result),
                Some(Waiter::Tree(view, element)) => self.ext_tree_answer(&view, &element, result),
                Some(Waiter::Hover(doc, pos)) => self.ext_hover_answer(doc, pos, result),
                Some(Waiter::Completion(seq)) => self.ext_completion_answer(seq, result),
                Some(Waiter::Definition) => self.ext_definition_answer(result),
                Some(Waiter::CodeActions { ext, auto }) => self.ext_code_actions_answer(&ext, auto, result),
                Some(Waiter::Formatting { path, version, save }) => {
                    let edits = result.map(|v| lsp::parse_text_edits(&v)).unwrap_or_default();
                    self.formatted(&path, version, edits, Encoding::Utf8, save);
                }
                Some(Waiter::Nobody) => {
                    if let Err(e) = result {
                        self.notify(Severity::Error, &e, &ext.display_name, Vec::new(), None);
                    }
                }
                None => {}
            },
            Incoming::Notification { method, params } => self.ext_message(ext, &method, params, None),
            Incoming::Request { id, method, params } => self.ext_message(ext, &method, params, Some(id)),
            Incoming::Log(line) => self.output.append(&ext.display_name, &format!("{line}\n")),
            Incoming::Exited => {
                let Some(i) = self.ext_host.procs.iter().position(|p| p.ext.id == ext.id) else { return };
                let p = self.ext_host.procs.remove(i);
                self.ext_forget(&ext.id);
                if !p.stopping {
                    let msg = format!("Extension '{}' stopped unexpectedly.", ext.display_name);
                    self.output.append(&ext.display_name, &format!("{msg}\n"));
                    self.notify(Severity::Error, &msg, &ext.display_name, Vec::new(), None);
                }
            }
        }
    }

    /// `initialize` was answered: send what was queued, then the open documents.
    fn ext_ready(&mut self, ext: &str) {
        let Some(p) = self.ext_proc(ext) else { return };
        p.ready = true;
        for msg in std::mem::take(&mut p.queued) {
            p.conn.send(msg);
        }
        let docs: Vec<Value> = self.ext_host.docs.keys().filter_map(|&id| self.docs.get(id)?.as_ref().map(|d| doc_json(d, false))).collect();
        for doc in docs {
            self.ext_notify(ext, "didOpenTextDocument", json!({ "document": doc }));
        }
        let editor = self.ext_active_editor(false);
        self.ext_notify(ext, "didChangeActiveTextEditor", json!({ "editor": editor }));
    }

    /// The active text editor as the protocol describes it (Null if none).
    fn ext_active_editor(&self, text: bool) -> Value {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return Value::Null };
        let Some(doc) = self.docs.get(ed.doc).and_then(|d| d.as_ref()) else { return Value::Null };
        let selections: Vec<Value> = ed.selections().iter().map(|s| range_json(doc, s)).collect();
        json!({ "document": doc_json(doc, text), "selections": selections })
    }

    /// A notification (`id` None) or request from an extension.
    fn ext_message(&mut self, ext: &Extension, method: &str, params: Value, id: Option<Value>) {
        let source = ext.display_name.clone();
        let answer = |wb: &mut Workbench, result: Result<Value, String>| {
            if let Some(id) = id.clone() {
                wb.ext_respond(&ext.id, id, result);
            }
        };
        match method {
            "window/showMessage" => {
                let severity = Severity::from_lsp(params["type"].as_u64().unwrap_or(3));
                self.notify(severity, params["message"].as_str().unwrap_or(""), &source, Vec::new(), None);
            }
            "window/showMessageRequest" => {
                let severity = Severity::from_lsp(params["type"].as_u64().unwrap_or(3));
                let actions: Vec<String> = params["actions"].as_array().into_iter().flatten().filter_map(|a| a["title"].as_str().map(String::from)).collect();
                let reply = id.clone().map(|id| (ext.id.clone(), id));
                self.notify(severity, params["message"].as_str().unwrap_or(""), &source, actions, reply);
            }
            "window/logMessage" => self.output.append(&source, &format!("{}\n", params["message"].as_str().unwrap_or(""))),
            "output/append" => {
                let channel = params["channel"].as_str().unwrap_or(&source).to_string();
                self.output.append(&channel, params["text"].as_str().unwrap_or(""));
            }
            "output/show" => {
                let channel = params["channel"].as_str().unwrap_or(&source).to_string();
                self.output.append(&channel, "");
                self.show_output_channel(&channel, !params["preserveFocus"].as_bool().unwrap_or(false));
            }
            "output/clear" => self.output.clear(params["channel"].as_str().unwrap_or(&source)),
            "window/setStatusBarItem" => {
                let item = StatusItem {
                    ext: ext.id.clone(),
                    id: params["id"].as_str().unwrap_or("").to_string(),
                    text: params["text"].as_str().unwrap_or("").to_string(),
                    command: params["command"].as_str().map(String::from),
                    right: params["alignment"].as_str() == Some("right"),
                    priority: params["priority"].as_i64().unwrap_or(0),
                };
                let status = &mut self.ext_host.status;
                match status.iter_mut().find(|s| s.ext == item.ext && s.id == item.id) {
                    Some(s) => *s = item,
                    None => status.push(item),
                }
            }
            "window/removeStatusBarItem" => {
                let item = params["id"].as_str().unwrap_or("");
                self.ext_host.status.retain(|s| !(s.ext == ext.id && s.id == item));
            }
            "treeView/refresh" => self.ext_tree_refresh(&ext.id, &params),
            "window/createTextEditorDecorationType" => self.ext_create_decoration_type(&ext.id, &params),
            "window/setDecorations" => self.ext_set_decorations(&ext.id, &params),
            "window/disposeDecorationType" => self.ext_dispose_decoration_type(&ext.id, &params),
            "languages/registerProvider" => self.ext_register_provider(&ext.id, &params),
            "languages/setDiagnostics" => self.ext_set_diagnostics(&ext.id, &params),
            "languages/clearDiagnostics" => self.ext_clear_diagnostics(&ext.id, &params),
            "commands/register" => {
                if let Some(c) = params["command"].as_str() {
                    commands::register_ext_command(c, None, &ext.id);
                }
            }
            "commands/execute" => {
                let command = params["command"].as_str().unwrap_or("").to_string();
                let args = params["arguments"].as_array().cloned().unwrap_or_default();
                match id {
                    Some(id) => self.ext_execute(&command, args, Some((ext.id.clone(), id))),
                    None => self.ext_execute(&command, args, None),
                }
            }
            "window/showQuickPick" | "window/showInputBox" => match id {
                Some(id) => self.ext_host.questions.push((ext.id.clone(), id, method.to_string(), params)),
                None => {}
            },
            "window/activeTextEditor" => {
                let editor = self.ext_active_editor(true);
                answer(self, Ok(editor));
            }
            "workspace/textDocument" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                let open = self.docs.iter().find_map(|d| d.as_ref().filter(|d| d.buffer.path() == Some(path.as_path())).map(|d| doc_json(d, true)));
                let doc = open.or_else(|| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    let lang = language::Lang::detect(Some(&path));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    Some(json!({ "path": path, "name": name, "languageId": lang.id(), "version": 0, "isDirty": false, "text": text }))
                });
                answer(self, Ok(doc.unwrap_or(Value::Null)));
            }
            "window/showTextDocument" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                self.open_file(&path);
                self.focus = super::Focus::Editor;
                if let (Some(range), Some((ed, doc))) = (params["selection"].as_object(), self.active_mut()) {
                    if doc.buffer.path() == Some(path.as_path()) {
                        let pos = |v: &Value| {
                            let line = (v["line"].as_u64().unwrap_or(0) as usize).min(doc.buffer.len_lines().saturating_sub(1));
                            Pos::new(line, Encoding::Utf8.from_lsp(&doc.buffer.line(line), v["character"].as_u64().unwrap_or(0) as u32))
                        };
                        let (a, z) = (pos(&range["start"]), pos(&range["end"]));
                        ed.jump_to(doc, a);
                        ed.set_selection(Selection { anchor: a, head: z, goal_col: None });
                    }
                }
                answer(self, Ok(Value::Null));
            }
            "workspace/applyEdit" => {
                let applied = lsp::parse_workspace_edit(&params["edit"]).is_some_and(|edit| self.apply_workspace_edit(&edit, Encoding::Utf8));
                answer(self, Ok(json!({ "applied": applied })));
            }
            "workspace/configuration" => {
                let key = params["key"].as_str().unwrap_or("");
                answer(self, Ok(self.ext_configuration(key)));
            }
            "workspace/updateConfiguration" => {
                let key = params["key"].as_str().unwrap_or("").to_string();
                let scope = if params["target"].as_str() == Some("workspace") { settings::Scope::Workspace } else { settings::Scope::User };
                let value = params.get("value").filter(|v| !v.is_null()).cloned();
                let result = self.settings.set(scope, &key, value).map(|_| Value::Null);
                if result.is_ok() {
                    self.apply_settings();
                }
                answer(self, result);
            }
            _ => answer(self, Err(format!("unknown method {method}"))),
        }
    }

    /// A setting's effective value, or an object of the settings under `key` (nested by the
    /// rest of their keys, as `getConfiguration(section)` gives them).
    fn ext_configuration(&self, key: &str) -> Value {
        if settings::schema::find(key).is_some() {
            return self.settings.get(key);
        }
        let prefix = format!("{key}.");
        let mut out = serde_json::Map::new();
        for s in settings::schema::all().iter().filter(|s| key.is_empty() || s.key.starts_with(&prefix)) {
            let rest = if key.is_empty() { s.key } else { &s.key[prefix.len()..] };
            let mut node = &mut out;
            let parts: Vec<&str> = rest.split('.').collect();
            for part in &parts[..parts.len() - 1] {
                node = node.entry(part.to_string()).or_insert_with(|| json!({})).as_object_mut().unwrap();
            }
            node.insert(parts[parts.len() - 1].to_string(), self.settings.get(s.key));
        }
        if out.is_empty() { self.settings.get(key) } else { Value::Object(out) }
    }

    /// Shows the next quick pick or input box; answers null to one that was closed.
    fn ext_questions_tick(&mut self) {
        if let Some(q) = &self.ext_host.question {
            let showing = self.palette.as_ref().is_some_and(|p| {
                p.input_box.as_ref().is_some_and(|b| matches!(b.purpose, super::GitInput::Extension))
                    || p.picker.as_ref().is_some_and(|pk| pk.choices.first().is_some_and(|c| matches!(c.action, Action::ExtPick(_))))
            });
            if showing {
                return;
            }
            let (ext, request) = (q.ext.clone(), q.request.clone());
            self.ext_host.question = None;
            self.ext_respond(&ext, request, Ok(Value::Null));
        }
        if self.palette.is_some() || self.ext_host.questions.is_empty() {
            return;
        }
        let (ext, request, method, params) = self.ext_host.questions.remove(0);
        if method == "window/showQuickPick" {
            let choices: Vec<Item> = params["items"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, item)| {
                    let (label, description, detail) = match item {
                        Value::String(s) => (s.clone(), String::new(), String::new()),
                        _ => (item["label"].as_str().unwrap_or("").to_string(), item["description"].as_str().unwrap_or("").to_string(), item["detail"].as_str().unwrap_or("").to_string()),
                    };
                    let detail = [description, detail].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" — ");
                    Item { label, detail, matches: Vec::new(), shortcut: None, action: Action::ExtPick(i), group: None, kind: None }
                })
                .collect();
            if choices.is_empty() {
                return self.ext_respond(&ext, request, Ok(Value::Null));
            }
            let placeholder = params["placeHolder"].as_str().unwrap_or("").to_string();
            self.palette = Some(Palette::with_picker(Picker { placeholder, choices }));
        } else {
            let title = params["title"].as_str().unwrap_or("");
            let prompt = params["prompt"].as_str().unwrap_or("");
            let prompt = match (title.is_empty(), prompt.is_empty()) {
                (true, true) => "Press 'Enter' to confirm your input or 'Escape' to cancel".to_string(),
                (false, true) => title.to_string(),
                (true, false) => format!("{prompt} (Press 'Enter' to confirm or 'Escape' to cancel)"),
                (false, false) => format!("{title}\n{prompt} (Press 'Enter' to confirm or 'Escape' to cancel)"),
            };
            let b = InputBox {
                prompt,
                placeholder: params["placeHolder"].as_str().unwrap_or("").to_string(),
                purpose: super::GitInput::Extension,
                error: None,
                password: params["password"].as_bool().unwrap_or(false),
            };
            self.palette = Some(Palette::with_input(b, params["value"].as_str().unwrap_or("")));
        }
        self.ext_host.question = Some(Question { ext, request });
    }

    /// The user answered the quick pick or input box an extension asked.
    pub(super) fn ext_answer(&mut self, answer: Value) {
        if let Some(q) = self.ext_host.question.take() {
            self.ext_respond(&q.ext, q.request, Ok(answer));
        }
    }

    /// Reports documents opened, changed and closed, and the active editor and selection.
    fn ext_documents_tick(&mut self) {
        let current: HashMap<usize, DocState> = self
            .docs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.as_ref().filter(|d| d.label.is_none() && (d.buffer.path().is_some() || d.untitled.is_some())).map(|d| (i, DocState { path: d.buffer.path().map(Path::to_path_buf), version: d.buffer.version() })))
            .collect();
        if self.ext_host.procs.is_empty() {
            self.ext_host.docs = current;
            self.ext_host.active = None;
            return;
        }
        let before = std::mem::take(&mut self.ext_host.docs);
        let doc = |wb: &Workbench, id: usize| wb.docs[id].as_ref().map(|d| doc_json(d, false));
        for (id, state) in &current {
            match before.get(id) {
                Some(old) if old == state => {}
                Some(old) if old.path == state.path => {
                    let d = doc(self, *id);
                    self.ext_broadcast("didChangeTextDocument", json!({ "document": d }));
                }
                old => {
                    if let Some(old) = old {
                        self.ext_broadcast("didCloseTextDocument", json!({ "document": { "path": old.path, "version": old.version } }));
                    }
                    let d = doc(self, *id);
                    self.ext_broadcast("didOpenTextDocument", json!({ "document": d }));
                }
            }
        }
        for (id, old) in &before {
            if !current.contains_key(id) {
                self.ext_broadcast("didCloseTextDocument", json!({ "document": { "path": old.path, "version": old.version } }));
            }
        }
        self.ext_host.docs = current;
        let active = self.active_editor().filter(|e| !e.is_special()).map(|e| (e.doc, e.selections()));
        if active != self.ext_host.active {
            let same_doc = active.as_ref().map(|a| a.0) == self.ext_host.active.as_ref().map(|a| a.0);
            self.ext_host.active = active;
            let editor = self.ext_active_editor(false);
            if same_doc && !editor.is_null() {
                self.ext_broadcast("didChangeTextEditorSelection", json!({ "editor": editor }));
            } else {
                self.ext_broadcast("didChangeActiveTextEditor", json!({ "editor": editor }));
            }
        }
    }

    pub(super) fn ext_saved(&mut self, path: &Path) {
        let Some(doc) = self.docs.iter().find_map(|d| d.as_ref().filter(|d| d.buffer.path() == Some(path))) else { return };
        let d = doc_json(doc, false);
        self.ext_broadcast("didSaveTextDocument", json!({ "document": d }));
    }

    pub(super) fn ext_settings_changed(&mut self) {
        if !self.ext_host.procs.is_empty() {
            self.ext_broadcast("didChangeConfiguration", json!({}));
        }
    }

    pub(super) fn ext_folders_changed(&mut self) {
        let folders = self.folders();
        self.ext_broadcast("didChangeWorkspaceFolders", json!({ "folders": folders }));
        if self.ext_host.started {
            self.ext_workspace_contains();
        }
    }

    /// Starts extensions with a `workspaceContains:` event that matches an open folder.
    fn ext_workspace_contains(&mut self) {
        let folders = self.folders();
        for e in crate::contributions::enabled() {
            if e.program().is_none() || self.ext_host.procs.iter().any(|p| p.ext.id == e.id) {
                continue;
            }
            let hit = e.activation_events().into_iter().filter_map(|a| a.strip_prefix("workspaceContains:").map(String::from)).find(|pattern| folders.iter().any(|f| folder_contains(f, pattern)));
            if let Some(pattern) = hit {
                self.ext_start(&e, &format!("workspaceContains:{pattern}"));
            }
        }
    }

    /// Whether an extension's program is running.
    pub(super) fn ext_running(&self, ext: &str) -> bool {
        self.ext_host.procs.iter().any(|p| p.ext.id == ext)
    }

    /// Clicking an extension's status bar item runs its command.
    pub(super) fn ext_status_click(&mut self, i: usize) {
        if let Some(command) = self.ext_host.status.get(i).and_then(|s| s.command.clone()) {
            self.ext_execute(&command, Vec::new(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(wb: &mut Workbench, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out waiting for {what}: log {:?}, toasts {:?}, status {:?}, running {}",
                wb.output.lines("Word Count"),
                wb.toast_list(),
                status(wb),
                wb.ext_host.procs.len()
            );
            std::thread::sleep(Duration::from_millis(10));
            wb.ext_tick();
        }
    }

    fn status(wb: &Workbench) -> String {
        wb.ext_host.status.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("|")
    }

    /// The example extension (`examples/extensions/word-count`, built by `cargo build`) against
    /// the host: activation, the status bar, commands, a quick pick, an input box, a notification
    /// button, an output channel, an edit, and disabling it.
    #[test]
    fn runs_the_word_count_example() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        if !root.join("target/debug/word-count").exists() {
            eprintln!("skipped: build the example first (cargo build -p word-count)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("orbvane-ext-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let ws = dir.join("ws").canonicalize().unwrap();
        let note = ws.join("notes.md");
        std::fs::write(&note, "one two three\ntwo four\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(ws.clone()), &[], std::sync::Arc::new(|| {}));
        let _ = wb.settings.set(settings::Scope::User, "wordCount.mode", None);
        // This thread's extensions: just the example, installed from where it is.
        let id = crate::contributions::with_mut(|r| {
            *r = extensions::Registry::scan(&dir.join("extensions"));
            r.install_folder(&root.join("examples/extensions/word-count"))
        })
        .unwrap();
        assert_eq!(id, "orbvane.word-count");
        crate::contributions::register(&crate::contributions::enabled()[0]);
        assert!(Command::from_id("wordCount.show").is_some());
        assert!(settings::schema::find("wordCount.mode").is_some());

        // Opening a Markdown file starts it (onLanguage:markdown); it counts in the status bar.
        wb.open_file(&note);
        wait_for(&mut wb, "the word count", |wb| status(wb).contains("5 Words"));
        assert!(wb.ext_running(&id));
        assert_eq!(status(&wb), "$(edit) 5 Words");

        // A command from the palette (by id, as keybindings run it): a notification.
        wb.run(Command::from_id("wordCount.show").unwrap());
        wait_for(&mut wb, "the notification", |wb| wb.toast_list().iter().any(|t| t.1.contains("notes.md: 5 words")));

        // A quick pick: Characters; the extension saves the setting and counts characters.
        wb.run(Command::from_id("wordCount.chooseMode").unwrap());
        wait_for(&mut wb, "the quick pick", |wb| wb.palette.as_ref().is_some_and(|p| p.items.len() == 3));
        wb.palette.as_mut().unwrap().selected = 1;
        wb.palette_accept();
        wait_for(&mut wb, "the characters", |wb| status(wb).contains("23 Characters"));
        assert_eq!(wb.settings.get("wordCount.mode"), json!("characters"));

        // Selecting text counts the selection.
        if let Some((ed, _)) = wb.active_mut() {
            ed.set_selection(Selection { anchor: Pos::new(0, 0), head: Pos::new(0, 3), goal_col: None });
        }
        wait_for(&mut wb, "the selection count", |wb| status(wb).contains("3 Characters Selected"));

        // An input box, then a notification with a button that fills an output channel.
        wb.ext_execute("wordCount.find", Vec::new(), None);
        wait_for(&mut wb, "the input box", |wb| wb.palette.as_ref().is_some_and(|p| p.input_box.is_some()));
        wb.palette.as_mut().unwrap().input = "two".into();
        wb.palette_accept();
        wait_for(&mut wb, "the answer", |wb| wb.toast_list().iter().any(|t| t.1.contains("'two' appears 2 times")));
        let (toast, _, actions) = wb.toast_list().into_iter().find(|t| t.1.contains("'two'")).unwrap();
        assert_eq!(actions, ["Show Lines"]);
        wb.close_toast(toast, Some(0));
        wait_for(&mut wb, "the output", |wb| wb.output.lines("Word Count").len() == 3);
        assert_eq!(wb.output.lines("Word Count")[1], "    1: one two three");
        assert_eq!(wb.output.active.as_deref(), Some("Word Count"));

        // An edit at the cursor.
        if let Some((ed, _)) = wb.active_mut() {
            ed.set_selection(Selection::caret(Pos::new(2, 0)));
        }
        wb.ext_execute("wordCount.insertSummary", Vec::new(), None);
        wait_for(&mut wb, "the edit", |wb| wb.active_doc().is_some_and(|d| d.buffer.text().contains("5 words, 23 characters, 2 lines")));

        // A command it doesn't have answers with an error notification.
        wb.ext_execute("wordCount.nope", Vec::new(), None);
        assert!(wb.toast_list().iter().any(|t| t.1 == "command 'wordCount.nope' not found"));

        // Disabling it stops the program and removes its status bar item and commands.
        wb.set_extension_enabled(&id, false);
        assert!(!wb.ext_running(&id) && status(&wb).is_empty());
        assert!(Command::from_id("wordCount.show").is_none());
        let _ = wb.settings.set(settings::Scope::User, "wordCount.mode", None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_workspace_files() {
        let dir = std::env::temp_dir().join(format!("orbvane-ext-contains-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("a/b/Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("node_modules/x/go.mod"), "").unwrap();
        assert!(folder_contains(&dir, "**/Cargo.toml"));
        assert!(!folder_contains(&dir, "**/go.mod"));
        assert!(folder_contains(&dir, "a/b/Cargo.toml"));
        assert!(!folder_contains(&dir, "Cargo.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
