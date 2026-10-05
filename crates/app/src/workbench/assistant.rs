//! The Assistant: a chat with a coding agent in the secondary sidebar. The agent is one of
//! the agents Orbvane knows (`crate::agents`, picked with `assistant.agent`) or any program that
//! speaks the Agent Client Protocol (`crate acp`), started from `assistant.agent.command`. It
//! reads and writes files through the editor (open documents included, unsaved changes and
//! all), asks before acting (`session/request_permission`: the options are buttons in the
//! transcript), and with `assistant.editorTools` it can ask our language servers about the code
//! (`mcp.rs`).
//!
//! Without an agent the Assistant shows the known ones with what their tools need (installed,
//! signed in: `agents::probe`), and installing or signing in runs in a task terminal.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use acp::update::{self, ToolContent, Update};
use acp::{Client, Incoming};
use render::{Canvas, Color, Rect, TextStyle};
use serde_json::{json, Value};

use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, SMALL, UI};
use crate::agents::{self, Choice, Probe};
use crate::icons;
use crate::palette::{Action, Item, Palette, Picker};
use crate::widgets::TextField;

/// The error code agents answer `session/new` with when the user has to sign in first.
const AUTH_REQUIRED: i64 = -32000;
const OUTPUT_CHANNEL: &str = "Assistant";
const INPUT_H: f32 = 30.0;
const PAD: f32 = 12.0;
/// How many of the agent's last stderr lines an error quotes.
const RECENT_LOG: usize = 4;

/// Something to do about the agent: from the setup screen, the agent menu, a picker or a
/// button in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AgentAction {
    /// Use this agent (`assistant.agent`).
    Use(&'static str),
    /// Install its tool, or sign in to it, in a terminal.
    Install(&'static str),
    SignIn(&'static str),
    /// Ask for a custom command.
    Custom,
    /// Show the agents to choose from.
    Choose,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Tool {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub content: Vec<ToolContent>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Permission {
    /// The request's id, to answer it.
    pub request: Value,
    pub title: String,
    /// (option id, name, kind).
    pub options: Vec<(String, String, String)>,
    /// The option picked (its name), or "Cancelled".
    pub answer: Option<String>,
    /// The changes it's about, to review.
    pub diffs: Vec<update::Diff>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool(Tool),
    Plan(Vec<update::PlanEntry>),
    Permission(Permission),
    /// Sign-in choices: (method id, name).
    Auth(Vec<(String, String)>),
    Notice(String),
    /// A message with buttons: what to do about the agent.
    Action(String, Vec<(String, AgentAction)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// No agent running.
    Off,
    /// Started; waiting for `initialize` and `session/new`.
    Starting,
    /// Waiting for the user to sign in (an `Entry::Auth`).
    SigningIn,
    Ready,
    /// A prompt is being answered.
    Working,
}

pub(super) struct Assistant {
    client: Option<Client>,
    pub phase: Phase,
    session: Option<String>,
    init_req: Option<i64>,
    session_req: Option<i64>,
    prompt_req: Option<i64>,
    auth_req: Option<i64>,
    /// Whether the agent takes file contents in prompts (`embeddedContext`).
    embedded: bool,
    agent_name: String,
    /// The sign-in methods the agent offers.
    auth_methods: Vec<(String, String)>,
    pub entries: Vec<Entry>,
    pub input: TextField,
    /// A prompt waiting for the session to start.
    queued: Option<Vec<Value>>,
    /// Pixels scrolled up from the bottom of the transcript.
    pub scroll: f32,
    /// The transcript's height in the last frame, and where it was drawn.
    content_h: f32,
    pub body: Rect,
    /// Whether the active file goes with the next message (the chip toggles it).
    pub send_file: bool,
    started: Option<Instant>,
    /// The folder the agent was started in.
    cwd: Option<PathBuf>,
    /// The agent it is (`Choice::key`).
    command: String,
    /// What the checks found about the known agents' tools, the ones being checked, and where
    /// the checks answer.
    pub probes: HashMap<&'static str, Probe>,
    probing: HashSet<&'static str>,
    probe_tx: Sender<(&'static str, Probe)>,
    probe_rx: Receiver<(&'static str, Probe)>,
    /// Install and sign-in terminals running: task label → agent id.
    tasks: HashMap<String, &'static str>,
    /// The agent's last lines on stderr.
    recent_log: VecDeque<String>,
    /// Where the agent menu opens (below its button).
    menu_at: (f32, f32),
    /// Tests: the shell command that stands in for a built-in agent's tool (`codex app-server`).
    pub server_override: Option<String>,
}

impl Default for Assistant {
    fn default() -> Self {
        let (probe_tx, probe_rx) = mpsc::channel();
        Self {
            client: None,
            phase: Phase::Off,
            session: None,
            init_req: None,
            session_req: None,
            prompt_req: None,
            auth_req: None,
            embedded: false,
            agent_name: String::new(),
            auth_methods: Vec::new(),
            entries: Vec::new(),
            input: TextField::default(),
            queued: None,
            scroll: 0.0,
            content_h: 0.0,
            body: Rect::default(),
            send_file: true,
            started: None,
            cwd: None,
            command: String::new(),
            probes: HashMap::new(),
            probing: HashSet::new(),
            probe_tx,
            probe_rx,
            tasks: HashMap::new(),
            recent_log: VecDeque::new(),
            menu_at: (0.0, 0.0),
            server_override: None,
        }
    }
}

/// The lines of `text` (from `path`'s 1-based `line`, at most `limit` of them).
fn slice_lines(text: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let start = line.unwrap_or(1).max(1) as usize - 1;
    let lines = text.split_inclusive('\n').skip(start);
    match limit {
        Some(n) => lines.take(n as usize).collect(),
        None => lines.collect(),
    }
}

impl Workbench {
    /// The agent the settings name.
    pub(super) fn agent_choice(&self) -> Choice {
        agents::choice(&self.settings.string("assistant.agent"), &self.settings.string("assistant.agent.command"))
    }

    /// The folder the agent works in.
    fn agent_cwd(&self) -> PathBuf {
        self.folder().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()))
    }

    fn assistant_notice(&mut self, text: impl Into<String>) {
        self.assistant.entries.push(Entry::Notice(text.into()));
        self.assistant.scroll = 0.0;
    }

    /// Starts the agent (if one is set up and it isn't running).
    pub(super) fn assistant_start(&mut self) {
        if self.assistant.client.is_some() {
            return;
        }
        match self.agent_choice() {
            Choice::None => {
                self.assistant.queued = None;
                self.assistant_choose_prompt("Choose the agent to talk to first.");
            }
            Choice::Custom(command) => self.start_custom_agent(&command),
            Choice::Builtin(a) => self.start_builtin_agent(a),
        }
    }

    /// One of our agents: its tool has to be installed and signed in.
    fn start_builtin_agent(&mut self, a: &'static agents::Agent) {
        if !self.agent_ready(a) {
            self.assistant.queued = None;
            return;
        }
        match a.id {
            "codex" => self.start_codex(),
            _ => self.start_claude(),
        }
    }

    /// Claude Code through our bridge (`acp::claude`), which runs `claude -p` with JSON in and
    /// out. The bridge adds its own arguments after these ("$@").
    fn start_claude(&mut self) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let command = self.assistant.server_override.clone().unwrap_or_else(|| "exec claude \"$@\"".into());
        let cwd = self.agent_cwd();
        let options = acp::Options { program: shell, args: vec!["-l".into(), "-c".into(), command, "claude".into()], cwd: cwd.clone(), env: self.mcp_env() };
        let version = self.assistant.probes.get("claude-code").map(|p| p.version.clone()).unwrap_or_default();
        match Client::in_process("claude", self.waker.clone(), move |rx, tx, log| acp::claude::serve(options, version, rx, tx, log)) {
            Ok(client) => self.agent_started(client, Choice::Builtin(agents::find("claude-code").unwrap()), cwd),
            Err(e) => self.assistant_notice(format!("Couldn't start Claude Code: {e}")),
        }
    }

    /// Codex through our bridge (`acp::codex`), which runs `codex app-server`.
    fn start_codex(&mut self) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let command = self.assistant.server_override.clone().unwrap_or_else(|| "exec codex app-server".into());
        let cwd = self.agent_cwd();
        let options = acp::Options { program: shell, args: vec!["-l".into(), "-c".into(), command], cwd: cwd.clone(), env: self.mcp_env() };
        match Client::in_process("codex", self.waker.clone(), move |rx, tx, log| acp::codex::serve(options, rx, tx, log)) {
            Ok(client) => self.agent_started(client, Choice::Builtin(agents::find("codex").unwrap()), cwd),
            Err(e) => self.assistant_notice(format!("Couldn't start Codex: {e}")),
        }
    }

    /// An agent is running: it gets `initialize`.
    fn agent_started(&mut self, mut client: Client, choice: Choice, cwd: PathBuf) {
        let a = &mut self.assistant;
        a.init_req = Some(client.initialize("orbvane", env!("CARGO_PKG_VERSION")));
        a.client = Some(client);
        a.phase = Phase::Starting;
        a.started = Some(Instant::now());
        a.cwd = Some(cwd);
        a.command = choice.key();
        a.recent_log.clear();
        self.output.append(OUTPUT_CHANNEL, &format!("Starting {}\n", choice.label()));
    }

    /// Whether `a`'s tool is installed and signed in, as far as the last check knows; if
    /// not, the transcript says what to do (a check still running counts as ready).
    fn agent_ready(&mut self, a: &'static agents::Agent) -> bool {
        let Some(p) = self.assistant.probes.get(a.id).cloned() else {
            self.assistant_probe(a);
            return true;
        };
        if p.path.is_none() {
            let text = format!("{} needs its command line tool, `{}`, which isn't installed.", a.name, a.program);
            self.assistant_action(text, vec![(format!("Install {}", a.name), AgentAction::Install(a.id)), ("Choose Another Agent".into(), AgentAction::Choose)]);
            return false;
        }
        if !p.signed_in {
            let text = format!("{} needs you to sign in first.", a.name);
            self.assistant_action(text, vec![("Sign In".into(), AgentAction::SignIn(a.id)), ("Choose Another Agent".into(), AgentAction::Choose)]);
            return false;
        }
        true
    }

    /// An Agent Client Protocol agent started with `command` through the login shell.
    fn start_custom_agent(&mut self, command: &str) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        // The login shell finds the agent the way a terminal would; exec so it gets the signals.
        let args = vec!["-l".to_string(), "-c".to_string(), format!("exec {command}")];
        let cwd = self.agent_cwd();
        let env = self.mcp_env();
        match Client::spawn(&shell, &args, &cwd, &env, self.waker.clone()) {
            Ok(client) => self.agent_started(client, Choice::Custom(command.to_string()), cwd),
            Err(e) => self.assistant_notice(format!("Couldn't start the agent ({command}): {e}")),
        }
    }

    /// Another folder opened: the agent works in one folder, so it stops (the next message
    /// starts it in the new one).
    pub(super) fn assistant_folders_changed(&mut self) {
        if self.assistant.client.is_some() && self.assistant.cwd.as_deref() != Some(self.agent_cwd().as_path()) {
            self.assistant_stop();
            self.assistant.entries.clear();
            self.assistant.scroll = 0.0;
        }
    }

    /// The agent settings changed: the next message starts the new agent.
    pub(super) fn assistant_settings_changed(&mut self) {
        let choice = self.agent_choice();
        if self.assistant.client.is_some() && self.assistant.command != choice.key() {
            self.assistant_stop();
            self.assistant_notice(format!("Switched to {}. The next message starts it.", choice.label()));
        }
    }

    /// A message with buttons in the transcript.
    fn assistant_action(&mut self, text: String, actions: Vec<(String, AgentAction)>) {
        self.assistant.entries.push(Entry::Action(text, actions));
        self.assistant.scroll = 0.0;
    }

    fn assistant_choose_prompt(&mut self, text: &str) {
        self.assistant_action(text.into(), vec![("Choose Agent".into(), AgentAction::Choose)]);
    }

    // ------------------------------------------------------------------ choosing an agent

    /// Checks `a`'s tool (installed? signed in?) in the background.
    fn assistant_probe(&mut self, a: &'static agents::Agent) {
        // Tests set `probes` themselves rather than run the real tools.
        if cfg!(test) {
            return;
        }
        if self.assistant.probing.insert(a.id) {
            let waker = self.waker.clone();
            agents::probe(a, self.assistant.probe_tx.clone(), move || waker());
        }
    }

    /// Checks every known agent that hasn't been checked.
    fn assistant_probe_all(&mut self) {
        for a in agents::AGENTS {
            if !self.assistant.probes.contains_key(a.id) {
                self.assistant_probe(a);
            }
        }
    }

    /// What the setup screen and pickers say about `a`.
    fn agent_status(&self, a: &agents::Agent) -> (String, AgentAction) {
        match self.assistant.probes.get(a.id) {
            _ if self.assistant.probing.contains(a.id) && !self.assistant.probes.contains_key(a.id) => ("Checking…".into(), AgentAction::Use(a.id)),
            None => (String::new(), AgentAction::Use(a.id)),
            Some(p) if p.path.is_none() => (format!("`{}` isn't installed", a.program), AgentAction::Install(a.id)),
            Some(p) if !p.signed_in => (format!("{} · not signed in", p.version), AgentAction::SignIn(a.id)),
            Some(p) => (format!("{} · signed in", p.version), AgentAction::Use(a.id)),
        }
    }

    /// What the setup screen's button for `a` does.
    pub(super) fn agent_setup_action(&self, a: &'static agents::Agent) -> AgentAction {
        self.agent_status(a).1
    }

    pub(crate) fn agent_action(&mut self, action: AgentAction) {
        match action {
            AgentAction::Use(id) => self.use_agent(id, settings::Scope::User),
            AgentAction::Install(id) => self.agent_task(id, true),
            AgentAction::SignIn(id) => self.agent_task(id, false),
            AgentAction::Custom => {
                let current = self.settings.string("assistant.agent.command");
                let purpose = super::GitInput::AgentCommand;
                self.open_input("The command that starts an agent that speaks the Agent Client Protocol (press Enter to confirm or Escape to cancel)", "Agent command", purpose, current.trim());
            }
            AgentAction::Choose => self.assistant_select_agent(),
        }
    }

    /// Makes `id` the agent (saved in `scope`), and says what it still needs.
    fn use_agent(&mut self, id: &'static str, scope: settings::Scope) {
        let Some(a) = agents::find(id) else { return };
        self.update_setting(scope, "assistant.agent", Some(Value::String(id.into())));
        self.apply_settings();
        // Old advice about choosing goes; the new agent's needs show when it starts.
        self.assistant.entries.retain(|e| !matches!(e, Entry::Action(..)));
        self.show_aux(super::aux_bar::AuxTab::Assistant);
        self.focus = Focus::Assistant;
        if self.assistant.probes.contains_key(id) {
            self.agent_ready(a);
        } else {
            self.assistant_probe(a);
        }
    }

    /// A custom command was typed.
    pub(super) fn set_custom_agent(&mut self, command: String) {
        self.update_setting(settings::Scope::User, "assistant.agent.command", Some(Value::String(command)));
        self.update_setting(settings::Scope::User, "assistant.agent", Some(Value::String("custom".into())));
        self.apply_settings();
        self.assistant.entries.retain(|e| !matches!(e, Entry::Action(..)));
        self.show_aux(super::aux_bar::AuxTab::Assistant);
        self.focus = Focus::Assistant;
    }

    /// Installs `id`'s tool (or signs in to it) in a task terminal; `assistant_task_done` follows.
    fn agent_task(&mut self, id: &'static str, install: bool) {
        let Some(a) = agents::find(id) else { return };
        let (label, command) = if install { (format!("Install {}", a.name), a.install) } else { (format!("Sign In to {}", a.name), a.sign_in) };
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
        match self.run_task_terminal(&label, command, &home, &[]) {
            Ok(()) => {
                self.assistant.tasks.insert(label, id);
            }
            Err(e) => self.assistant_notice(e),
        }
    }

    /// A task ended: if it was an install or sign-in, the agent is checked again. Returns
    /// whether it was one.
    pub(super) fn assistant_task_done(&mut self, label: &str, code: Option<i32>) -> bool {
        let Some(id) = self.assistant.tasks.remove(label) else { return false };
        let Some(a) = agents::find(id) else { return true };
        if code != Some(0) {
            self.assistant_notice(format!("{label} didn't finish. The terminal shows what happened."));
        }
        self.assistant.probes.remove(id);
        self.assistant.entries.retain(|e| !matches!(e, Entry::Action(..)));
        self.assistant_probe(a);
        true
    }

    /// Assistant: Select Agent: the agents in the palette.
    pub(super) fn assistant_select_agent(&mut self) {
        self.assistant_probe_all();
        let current = self.agent_choice();
        let mut choices: Vec<Item> = agents::AGENTS
            .iter()
            .map(|a| {
                let (status, _) = self.agent_status(a);
                let mark = if current == Choice::Builtin(a) { " (current)" } else { "" };
                Item { label: format!("{}{mark}", a.name), detail: status, matches: Vec::new(), shortcut: None, action: Action::Agent(AgentAction::Use(a.id)), group: None, kind: None }
            })
            .collect();
        let custom = match &current {
            Choice::Custom(command) => format!("{command} (current)"),
            _ => "Any agent that speaks the Agent Client Protocol".into(),
        };
        choices.push(Item { label: "Custom Command…".into(), detail: custom, matches: Vec::new(), shortcut: None, action: Action::Agent(AgentAction::Custom), group: None, kind: None });
        self.palette = Some(Palette::with_picker(Picker { placeholder: "Select the agent the Assistant talks to".into(), choices }));
    }

    /// The agent menu below the message box.
    pub(super) fn assistant_agent_menu(&mut self) {
        self.assistant_probe_all();
        let current = self.agent_choice();
        let mut entries: Vec<(PopupItem, PopupAction)> = agents::AGENTS
            .iter()
            .map(|a| (PopupItem::Item { label: a.name.into(), enabled: true, checked: Some(current == Choice::Builtin(a)) }, PopupAction::Agent(AgentAction::Use(a.id))))
            .collect();
        entries.push((PopupItem::Separator, PopupAction::None));
        let custom = matches!(current, Choice::Custom(_));
        entries.push((PopupItem::Item { label: "Custom Command…".into(), enabled: true, checked: Some(custom) }, PopupAction::Agent(AgentAction::Custom)));
        if let Choice::Builtin(a) = current {
            entries.push((PopupItem::Separator, PopupAction::None));
            entries.push((PopupItem::Item { label: format!("Sign In to {}…", a.name), enabled: true, checked: None }, PopupAction::Agent(AgentAction::SignIn(a.id))));
        }
        let (x, y) = self.assistant.menu_at;
        self.show_popup(entries, x, y);
    }

    /// Stops the agent (quitting, or the command changed).
    pub(super) fn assistant_stop(&mut self) {
        if let Some(mut c) = self.assistant.client.take() {
            c.shutdown();
        }
        let a = &mut self.assistant;
        a.phase = Phase::Off;
        a.session = None;
        a.prompt_req = None;
        a.session_req = None;
        a.init_req = None;
        a.auth_req = None;
    }

    fn new_session(&mut self) {
        let cwd = self.agent_cwd();
        let servers = self.mcp_servers();
        if let Some(c) = &mut self.assistant.client {
            self.assistant.session_req = Some(c.request("session/new", json!({ "cwd": cwd, "mcpServers": servers })));
        }
    }

    /// New Chat: a fresh session (the agent keeps running) and an empty transcript.
    pub(super) fn assistant_new_chat(&mut self) {
        self.assistant_cancel();
        self.assistant.entries.clear();
        self.assistant.scroll = 0.0;
        self.assistant.session = None;
        if self.assistant.client.is_some() && self.assistant.phase != Phase::Starting {
            self.assistant.phase = Phase::Starting;
            self.new_session();
        }
    }

    /// Stops the current answer; open permission questions are answered "cancelled".
    pub(super) fn assistant_cancel(&mut self) {
        let a = &mut self.assistant;
        if a.phase != Phase::Working {
            return;
        }
        let Some(c) = &mut a.client else { return };
        if let Some(session) = &a.session {
            c.notify("session/cancel", json!({ "sessionId": session }));
        }
        for e in &mut a.entries {
            if let Entry::Permission(p) = e {
                if p.answer.is_none() {
                    c.respond(p.request.clone(), json!({ "outcome": { "outcome": "cancelled" } }));
                    p.answer = Some("Cancelled".into());
                }
            }
        }
    }

    /// Sends what's typed, with the active file and selection as context.
    pub(super) fn assistant_send(&mut self) {
        let text = self.assistant.input.text.trim().to_string();
        if text.is_empty() || self.assistant.phase == Phase::Working {
            return;
        }
        if self.agent_choice() == Choice::None {
            self.assistant_choose_prompt("Choose the agent to talk to first.");
            return;
        }
        if matches!(self.agent_choice(), Choice::Builtin(_)) && self.settings.bool("assistant.saveBeforeSending") {
            self.save_for_agent();
        }
        self.assistant.input.set_text("");
        self.assistant.entries.push(Entry::User(text.clone()));
        self.assistant.scroll = 0.0;
        let prompt = self.assistant_prompt(&text);
        if self.assistant.phase == Phase::Ready {
            self.send_prompt(prompt);
        } else {
            self.assistant.queued = Some(prompt);
            self.assistant_start();
        }
    }

    /// Saves the documents in the agent's folder that have unsaved changes: built-in agents read
    /// files from disk.
    fn save_for_agent(&mut self) {
        let cwd = self.agent_cwd();
        let dirty: Vec<usize> = (0..self.docs.len())
            .filter(|&i| self.docs[i].as_ref().is_some_and(|d| d.buffer.is_dirty() && d.buffer.path().is_some_and(|p| p.starts_with(&cwd))))
            .collect();
        for id in dirty {
            self.save_doc_quietly(id);
        }
    }

    /// The prompt's content blocks: the text, then the active file (a link, or its selection
    /// embedded when the agent takes file contents).
    fn assistant_prompt(&self, text: &str) -> Vec<Value> {
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        if !(self.assistant.send_file && self.settings.bool("assistant.sendActiveFile")) {
            return blocks;
        }
        let Some((ed, doc)) = self.active_editor().filter(|e| !e.is_special()).and_then(|e| Some((e, self.docs[e.doc].as_ref()?))) else { return blocks };
        let Some(path) = doc.buffer.path() else { return blocks };
        let uri = lsp::path_to_uri(path);
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let (a, b) = ed.sel.ordered();
        if a != b {
            let selected = doc.buffer.text_in(&ed.sel);
            let note = format!("Selected lines {}-{} of {}:", a.line + 1, b.line + 1, path.display());
            if self.assistant.embedded {
                blocks.push(json!({ "type": "text", "text": note }));
                blocks.push(json!({ "type": "resource", "resource": { "uri": format!("{uri}#L{}-{}", a.line + 1, b.line + 1), "text": selected, "mimeType": "text/plain" } }));
            } else {
                blocks.push(json!({ "type": "text", "text": format!("{note}\n```\n{selected}\n```") }));
            }
        }
        blocks.push(json!({ "type": "resource_link", "uri": uri, "name": name }));
        blocks
    }

    fn send_prompt(&mut self, prompt: Vec<Value>) {
        let a = &mut self.assistant;
        let (Some(c), Some(session)) = (&mut a.client, &a.session) else { return };
        a.prompt_req = Some(c.request("session/prompt", json!({ "sessionId": session, "prompt": prompt })));
        a.phase = Phase::Working;
    }

    /// Handles what the agent sent. Called every frame.
    pub(super) fn assistant_tick(&mut self) {
        while let Ok((id, probe)) = self.assistant.probe_rx.try_recv() {
            self.assistant.probing.remove(id);
            self.assistant.probes.insert(id, probe);
            // Checked after Use or an install: say what's still missing.
            if let (Choice::Builtin(a), true) = (self.agent_choice(), self.assistant.client.is_none()) {
                if a.id == id && !self.assistant.entries.iter().any(|e| matches!(e, Entry::Action(..))) && !self.assistant.entries.is_empty() {
                    self.agent_ready(a);
                }
            }
        }
        if self.assistant.client.is_none() {
            return;
        }
        let messages = self.assistant.client.as_mut().map(Client::poll).unwrap_or_default();
        for msg in messages {
            match msg {
                Incoming::Response { id, result } => self.assistant_response(id, result),
                Incoming::Notification { method, params } if method == "session/update" => {
                    if params["sessionId"].as_str() == self.assistant.session.as_deref() {
                        self.assistant_update(update::parse(&params["update"]));
                    }
                }
                Incoming::Notification { .. } => {}
                Incoming::Request { id, method, params } => self.assistant_request(id, &method, &params),
                Incoming::Log(line) => {
                    self.output.append(OUTPUT_CHANNEL, &format!("{line}\n"));
                    let recent = &mut self.assistant.recent_log;
                    if !line.trim().is_empty() {
                        recent.push_back(line);
                        if recent.len() > RECENT_LOG {
                            recent.pop_front();
                        }
                    }
                }
                Incoming::Exited => {
                    let starting = self.assistant.phase == Phase::Starting && self.assistant.init_req.is_some();
                    self.assistant_stop();
                    if starting && matches!(self.agent_choice(), Choice::Custom(_)) {
                        self.agent_exited_starting();
                    } else if starting {
                        let printed: Vec<String> = self.assistant.recent_log.iter().cloned().collect();
                        let mut text = format!("{} stopped while starting.", self.agent_choice().label());
                        if !printed.is_empty() {
                            text.push_str(&format!(" It printed:\n{}", printed.join("\n")));
                        }
                        self.assistant_notice(text);
                    } else {
                        self.assistant_notice("The agent exited. Send a message to start it again.");
                    }
                    return;
                }
            }
        }
    }

    /// A custom agent ended before answering `initialize`: most likely a program that doesn't
    /// speak the protocol (an agent's interactive tool), so say so and offer the agents we know.
    fn agent_exited_starting(&mut self) {
        let command = match self.agent_choice() {
            Choice::Custom(c) => c,
            _ => String::new(),
        };
        let program = Choice::Custom(command.clone()).label();
        let mut text = format!("`{program}` stopped before answering, so it doesn't seem to speak the Agent Client Protocol.");
        let printed: Vec<String> = self.assistant.recent_log.iter().cloned().collect();
        if !printed.is_empty() {
            text.push_str(&format!(" It printed:\n{}", printed.join("\n")));
        }
        let mut actions = Vec::new();
        if let Some(a) = agents::AGENTS.iter().find(|a| a.program == program) {
            text.push_str(&format!("\n{} is built in: use it instead.", a.name));
            actions.push((format!("Use {}", a.name), AgentAction::Use(a.id)));
        }
        actions.push(("Choose Agent".into(), AgentAction::Choose));
        self.assistant_action(text, actions);
    }

    fn assistant_response(&mut self, id: i64, result: Result<Value, (i64, String)>) {
        let a = &mut self.assistant;
        if Some(id) == a.init_req {
            a.init_req = None;
            match result {
                Ok(r) => {
                    a.embedded = r["agentCapabilities"]["promptCapabilities"]["embeddedContext"].as_bool() == Some(true);
                    a.agent_name = r["agentInfo"]["title"].as_str().or(r["agentInfo"]["name"].as_str()).unwrap_or("").to_string();
                    let methods: Vec<(String, String)> = r["authMethods"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|m| Some((m["id"].as_str()?.to_string(), m["name"].as_str()?.to_string())))
                        .collect();
                    // Kept for when session/new asks for a sign-in.
                    a.entries.retain(|e| !matches!(e, Entry::Auth(_)));
                    a.auth_methods = methods;
                    self.new_session();
                }
                Err((_, e)) => {
                    self.assistant_stop();
                    self.assistant_notice(format!("The agent didn't start: {e}"));
                }
            }
        } else if Some(id) == a.session_req {
            a.session_req = None;
            match result {
                Ok(r) => {
                    a.session = r["sessionId"].as_str().map(String::from);
                    a.phase = Phase::Ready;
                    if let Some(prompt) = a.queued.take() {
                        self.send_prompt(prompt);
                    }
                }
                Err((code, e)) if code == AUTH_REQUIRED && !a.auth_methods.is_empty() => {
                    a.phase = Phase::SigningIn;
                    let methods = a.auth_methods.clone();
                    self.assistant.entries.push(Entry::Auth(methods));
                    let _ = e;
                }
                Err((code, e)) => {
                    a.phase = Phase::Ready;
                    a.queued = None;
                    match self.agent_choice() {
                        // One of ours that isn't signed in: the button signs in.
                        Choice::Builtin(agent) if code == AUTH_REQUIRED => {
                            self.assistant.probes.remove(agent.id);
                            self.assistant_action(e, vec![("Sign In".into(), AgentAction::SignIn(agent.id))]);
                        }
                        _ => self.assistant_notice(format!("The agent couldn't start a session: {e}")),
                    }
                }
            }
        } else if Some(id) == a.auth_req {
            a.auth_req = None;
            match result {
                Ok(_) => {
                    a.entries.retain(|e| !matches!(e, Entry::Auth(_)));
                    a.phase = Phase::Starting;
                    self.new_session();
                }
                Err((_, e)) => {
                    a.phase = Phase::SigningIn;
                    self.assistant_notice(format!("Signing in didn't work: {e}"));
                }
            }
        } else if Some(id) == a.prompt_req {
            a.prompt_req = None;
            a.phase = Phase::Ready;
            match result {
                Ok(r) => match r["stopReason"].as_str().unwrap_or("end_turn") {
                    "end_turn" => {}
                    "cancelled" => self.assistant_notice("Stopped."),
                    "max_tokens" => self.assistant_notice("The agent stopped: it reached its output limit."),
                    "max_turn_requests" => self.assistant_notice("The agent stopped: it reached its limit of steps for one message."),
                    "refusal" => self.assistant_notice("The agent declined to continue."),
                    other => self.assistant_notice(format!("The agent stopped ({other}).")),
                },
                Err((code, e)) => match self.agent_choice() {
                    // One of ours whose sign-in expired: the button signs in again.
                    Choice::Builtin(agent) if code == AUTH_REQUIRED => {
                        self.assistant.probes.remove(agent.id);
                        self.assistant_action(e, vec![("Sign In".into(), AgentAction::SignIn(agent.id))]);
                    }
                    _ => self.assistant_notice(format!("The agent reported an error: {e}")),
                },
            }
        }
    }

    /// Picks a sign-in method.
    pub(super) fn assistant_authenticate(&mut self, method: &str) {
        let a = &mut self.assistant;
        if let Some(c) = &mut a.client {
            a.auth_req = Some(c.request("authenticate", json!({ "methodId": method })));
        }
    }

    fn assistant_update(&mut self, u: Update) {
        let entries = &mut self.assistant.entries;
        match u {
            Update::AgentText(t) => match entries.last_mut() {
                Some(Entry::Agent(s)) => s.push_str(&t),
                // A paragraph break before the first text after a tool call means nothing.
                _ => entries.push(Entry::Agent(t.trim_start_matches('\n').to_string())),
            },
            Update::Thought(t) => match entries.last_mut() {
                Some(Entry::Thought(s)) => s.push_str(&t),
                _ => entries.push(Entry::Thought(t)),
            },
            Update::ToolCall(call) => entries.push(Entry::Tool(Tool {
                id: call.id,
                title: call.title.unwrap_or_default(),
                kind: call.kind.unwrap_or_else(|| "other".into()),
                status: call.status.unwrap_or_else(|| "pending".into()),
                content: call.content.unwrap_or_default(),
            })),
            Update::ToolCallUpdate(call) => {
                let found = entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool(t) if t.id == call.id => Some(t),
                    _ => None,
                });
                match found {
                    Some(t) => {
                        if let Some(v) = call.title {
                            t.title = v;
                        }
                        if let Some(v) = call.kind {
                            t.kind = v;
                        }
                        if let Some(v) = call.status {
                            t.status = v;
                        }
                        if let Some(v) = call.content {
                            t.content = v;
                        }
                    }
                    None => entries.push(Entry::Tool(Tool {
                        id: call.id,
                        title: call.title.unwrap_or_default(),
                        kind: call.kind.unwrap_or_else(|| "other".into()),
                        status: call.status.unwrap_or_else(|| "in_progress".into()),
                        content: call.content.unwrap_or_default(),
                    })),
                }
            }
            Update::Plan(plan) => match entries.iter_mut().rev().find(|e| matches!(e, Entry::Plan(_))) {
                Some(Entry::Plan(p)) => *p = plan,
                _ => entries.push(Entry::Plan(plan)),
            },
            Update::UserText(_) | Update::Commands(_) | Update::Other(_) => {}
        }
    }

    fn assistant_request(&mut self, id: Value, method: &str, params: &Value) {
        match method {
            "session/request_permission" => {
                let call = update::permission_tool_call(params);
                // The title from the request, else from the tool call it's about.
                let known = self.assistant.entries.iter().rev().find_map(|e| match e {
                    Entry::Tool(t) if t.id == call.id => Some(t.clone()),
                    _ => None,
                });
                let title = call.title.clone().or_else(|| known.as_ref().map(|t| t.title.clone())).unwrap_or_else(|| "The agent wants to continue".into());
                let content = call.content.clone().or_else(|| known.map(|t| t.content)).unwrap_or_default();
                let diffs = content.into_iter().filter_map(|c| if let ToolContent::Diff(d) = c { Some(d) } else { None }).collect();
                self.assistant.entries.push(Entry::Permission(Permission { request: id, title, options: update::permission_options(params), answer: None, diffs }));
                self.assistant.scroll = 0.0;
            }
            "fs/read_text_file" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                let text = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path.as_path())).map(|d| d.buffer.text());
                let text = match text {
                    Some(t) => Ok(t),
                    None => std::fs::read_to_string(&path).map_err(|e| e.to_string()),
                };
                let reply = text.map(|t| slice_lines(&t, params["line"].as_u64(), params["limit"].as_u64()));
                let c = self.assistant.client.as_mut();
                match (c, reply) {
                    (Some(c), Ok(t)) => c.respond(id, json!({ "content": t })),
                    (Some(c), Err(e)) => c.respond_error(id, -32002, &format!("{}: {e}", path.display())),
                    _ => {}
                }
            }
            "fs/write_text_file" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                let result = self.agent_write(&path, params["content"].as_str().unwrap_or(""));
                if let Some(c) = &mut self.assistant.client {
                    match result {
                        Ok(()) => c.respond(id, Value::Null),
                        Err(e) => c.respond_error(id, -32603, &format!("{}: {e}", path.display())),
                    }
                }
            }
            _ => {
                if let Some(c) = &mut self.assistant.client {
                    c.respond_error(id, -32601, &format!("{method} isn't supported"));
                }
            }
        }
    }

    /// The agent writes `path`: an open document is edited (one undo step, cursors kept) and
    /// saved unless it had unsaved changes of its own; other files are written to disk.
    fn agent_write(&mut self, path: &Path, content: &str) -> Result<(), String> {
        let open = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path)));
        let Some(id) = open else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            return std::fs::write(path, content).map_err(|e| e.to_string());
        };
        let doc = self.docs[id].as_ref().unwrap();
        let was_dirty = doc.buffer.is_dirty();
        let old = doc.buffer.text();
        if old == content {
            return Ok(());
        }
        // The smallest edit: what's between the common prefix and suffix.
        let (oc, nc): (Vec<char>, Vec<char>) = (old.chars().collect(), content.chars().collect());
        let prefix = oc.iter().zip(&nc).take_while(|(a, b)| a == b).count();
        let suffix = oc[prefix..].iter().rev().zip(nc[prefix..].iter().rev()).take_while(|(a, b)| a == b).count();
        let (start, end) = (doc.buffer.pos_of(prefix), doc.buffer.pos_of(oc.len() - suffix));
        let insert: String = nc[prefix..nc.len() - suffix].iter().collect();
        self.edit_doc(id, vec![(start, end, insert)], false);
        if !was_dirty && !self.save_doc_quietly(id) {
            return Err("couldn't save it".into());
        }
        Ok(())
    }

    /// Answers permission request `entry` with its option `option`.
    pub(super) fn assistant_answer(&mut self, entry: usize, option: usize) {
        let a = &mut self.assistant;
        let Some(Entry::Permission(p)) = a.entries.get_mut(entry) else { return };
        if p.answer.is_some() {
            return;
        }
        let Some((id, name, _)) = p.options.get(option).cloned() else { return };
        if let Some(c) = &mut a.client {
            c.respond(p.request.clone(), json!({ "outcome": { "outcome": "selected", "optionId": id } }));
        }
        p.answer = Some(name);
    }

    /// Shows a proposed change as a diff tab: the file as it is ↔ as the agent would have it.
    pub(super) fn assistant_review(&mut self, entry: usize, diff: usize) {
        let d = match self.assistant.entries.get(entry) {
            Some(Entry::Permission(p)) => p.diffs.get(diff).cloned(),
            Some(Entry::Tool(t)) => t.content.iter().filter_map(|c| if let ToolContent::Diff(d) = c { Some(d.clone()) } else { None }).nth(diff),
            _ => None,
        };
        let Some(d) = d else { return };
        let path = PathBuf::from(&d.path);
        let old = d.old_text.clone().unwrap_or_default();
        self.open_text_diff(&path, &old, &d.new_text, "proposed");
    }

    pub(super) fn assistant_deadline(&self) -> Option<Instant> {
        // The working spinner turns.
        (self.assistant.phase == Phase::Working || self.assistant.phase == Phase::Starting).then(|| Instant::now() + Duration::from_millis(120))
    }

    pub(super) fn assistant_scroll(&mut self, dy: f32) {
        let a = &mut self.assistant;
        let max = (a.content_h - a.body.h).max(0.0);
        a.scroll = (a.scroll + dy).clamp(0.0, max);
    }

    pub(super) fn assistant_key(&mut self, k: &crate::input::KeyInput) {
        use crate::input::Key;
        match k.key {
            Key::Enter => self.assistant_send(),
            Key::Escape if self.assistant.phase == Phase::Working => self.assistant_cancel(),
            Key::Escape => self.focus = Focus::Editor,
            _ => {
                self.assistant.input.key(k);
            }
        }
    }

    pub(super) fn assistant_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let f = &mut self.assistant.input;
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.assistant.input.insert(&text);
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn draw_assistant(&mut self, c: &mut Canvas, r: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let dim = self.color("descriptionForeground");
        if self.agent_choice() == Choice::None && self.assistant.entries.is_empty() {
            return self.draw_assistant_setup(c, r, fg, dim);
        }
        let (body, footer) = r.cut_bottom(INPUT_H + 40.0);
        self.draw_transcript(c, body, fg, dim);
        self.draw_assistant_input(c, footer, fg, dim);
    }

    /// No agent yet: the agents we know, each with what it needs, and a custom command.
    fn draw_assistant_setup(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        self.assistant_probe_all();
        let mut y = r.y + 16.0;
        let w = r.w - 2.0 * PAD;
        let x = r.x + PAD;
        let head = TextStyle::ui(14.0, fg).weight(600);
        c.text(x, y, "Choose your coding agent", &head);
        y += 26.0;
        let text = "The Assistant works with a coding agent in this folder. It reads and edits files through the editor (unsaved changes included), \
                    asks before it changes anything or runs a command, and can ask the language servers about your code.";
        let body = TextStyle::ui(UI, dim);
        for line in super::intel::wrap(c, text, &body, w) {
            c.text(x, y, &line, &body);
            y += 18.0;
        }
        y += 12.0;
        let name_st = TextStyle::ui(UI, fg).weight(600);
        let small = TextStyle::ui(SMALL, dim);
        for (i, a) in agents::AGENTS.iter().enumerate() {
            // The name and button on top, then what it is and what its tool needs.
            let about = super::intel::wrap(c, a.about, &small, w - 24.0);
            let card = Rect::new(x, y, w, 50.0 + 16.0 * about.len() as f32 + 20.0);
            c.bordered(card, self.color("editorWidget.background"), self.color("widget.border"), 1.0, 8.0);
            let (status, action) = self.agent_status(a);
            let label = match action {
                AgentAction::Install(_) => "Install",
                AgentAction::SignIn(_) => "Sign In",
                _ => "Use",
            };
            let button = Rect::new(card.right() - 10.0 - 76.0, y + 10.0, 76.0, 26.0);
            self.option_button(c, button, label, true, Hit::AssistantAgent(i as u8));
            c.text_fit(Rect::new(x + 12.0, y + 13.0, button.x - x - 20.0, 20.0), a.name, &name_st);
            let mut ly = y + 46.0;
            for line in &about {
                c.text(x + 12.0, ly, line, &small);
                ly += 16.0;
            }
            let status_st = TextStyle::ui(SMALL, if matches!(action, AgentAction::Use(_)) { dim } else { self.color("editorWarning.foreground") });
            c.text_fit(Rect::new(x + 12.0, ly + 2.0, card.w - 24.0, 16.0), &status, &status_st);
            y += card.h + 10.0;
        }
        y += 4.0;
        let link = TextStyle::ui(UI, self.color("textLink.foreground"));
        let label = "Use another agent with a custom command…";
        let lw = c.measure(label, &link).min(w);
        let rect = Rect::new(x, y, lw, 20.0);
        c.text_fit(rect, label, &link);
        self.hits.push((rect, Hit::AssistantCustomAgent));
    }

    /// One entry's lines and look, for layout: (text, style, indent, background).
    fn entry_lines(&self, c: &mut Canvas, i: usize, e: &Entry, w: f32, fg: Color, dim: Color) -> Vec<(String, TextStyle)> {
        let ui = TextStyle::ui(UI, fg);
        let small = TextStyle::ui(SMALL, dim);
        let mono = TextStyle::mono(12.0, 18.0, fg);
        let wrap = |c: &mut Canvas, t: &str, st: &TextStyle, w: f32| super::intel::wrap(c, t, st, w).into_iter().map(|l| (l, st.clone())).collect::<Vec<_>>();
        let _ = i;
        match e {
            Entry::User(t) => wrap(c, t, &ui, w - 20.0),
            Entry::Agent(t) => {
                // Code fences in the editor font; the rest wrapped.
                let mut out = Vec::new();
                let mut code = false;
                for line in t.split('\n') {
                    if line.trim_start().starts_with("```") {
                        code = !code;
                        continue;
                    }
                    if code {
                        out.push((line.replace('\t', "    "), mono.clone()));
                    } else {
                        out.extend(wrap(c, line, &ui, w));
                    }
                }
                out
            }
            Entry::Thought(t) => {
                let mut lines = wrap(c, t.trim(), &TextStyle::ui(SMALL, dim).italic(true), w - 12.0);
                if lines.len() > 4 {
                    lines.truncate(4);
                    lines.push(("…".into(), small.clone()));
                }
                lines
            }
            Entry::Tool(t) => {
                let mut lines = wrap(c, if t.title.is_empty() { &t.kind } else { &t.title }, &ui, w - 24.0);
                for content in &t.content {
                    match content {
                        ToolContent::Diff(d) => lines.push((diff_summary(d), small.clone())),
                        ToolContent::Text(s) if !s.trim().is_empty() => {
                            let mut text: Vec<_> = s.lines().take(3).map(|l| (l.to_string(), TextStyle::mono(11.0, 16.0, dim))).collect();
                            if s.lines().count() > 3 {
                                text.push(("…".into(), small.clone()));
                            }
                            lines.extend(text);
                        }
                        _ => {}
                    }
                }
                lines
            }
            Entry::Plan(p) => p.iter().map(|e| (e.content.clone(), if e.status == "completed" { TextStyle::ui(UI, dim) } else { ui.clone() })).collect(),
            Entry::Permission(p) => {
                let mut lines = wrap(c, &p.title, &ui.clone().weight(600), w - 20.0);
                for d in &p.diffs {
                    lines.push((diff_summary(d), small.clone()));
                }
                lines
            }
            Entry::Auth(_) => wrap(c, "The agent needs you to sign in:", &ui, w - 20.0),
            Entry::Notice(t) => wrap(c, t, &small, w),
            Entry::Action(t, _) => t.split('\n').flat_map(|l| wrap(c, l, &ui, w - 20.0)).collect(),
        }
    }

    fn draw_transcript(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        self.assistant.body = r;
        self.hits.push((r, Hit::AssistantBody));
        c.push_clip(r);
        let w = r.w - 2.0 * PAD;
        if self.assistant.entries.is_empty() {
            let name = if self.assistant.agent_name.is_empty() { "the agent".to_string() } else { self.assistant.agent_name.clone() };
            let hint = format!("Ask {name} about this project, or to change something. It sees the file you're in.");
            let mut y = r.y + 16.0;
            for line in super::intel::wrap(c, &hint, &TextStyle::ui(UI, dim), w) {
                c.text(r.x + PAD, y, &line, &TextStyle::ui(UI, dim));
                y += 18.0;
            }
            self.assistant.content_h = 0.0;
            c.pop_clip();
            return;
        }
        // Lay out every entry, then draw from the bottom up (scrolled by `scroll`).
        let entries = self.assistant.entries.clone();
        let mut blocks = Vec::new();
        let mut total = 8.0;
        for (i, e) in entries.iter().enumerate() {
            let lines = self.entry_lines(c, i, e, w, fg, dim);
            let line_h: f32 = lines.iter().map(|(_, st)| st.line_height.max(16.0)).sum();
            let extra = match e {
                Entry::User(_) => 16.0,
                Entry::Tool(t) => 10.0 + if t.content.iter().any(|c| matches!(c, ToolContent::Diff(_))) { 26.0 } else { 0.0 },
                Entry::Permission(p) => 20.0 + 32.0 * p.options.len().div_ceil(2).max(1) as f32 + if p.diffs.is_empty() { 0.0 } else { 30.0 },
                Entry::Auth(m) => 20.0 + 32.0 * m.len() as f32,
                Entry::Action(_, actions) => 20.0 + 32.0 * actions.len() as f32,
                Entry::Plan(_) => 12.0,
                _ => 4.0,
            };
            let h = line_h + extra;
            blocks.push((total, h, lines));
            total += h + 10.0;
        }
        self.assistant.content_h = total;
        let max = (total - r.h).max(0.0);
        self.assistant.scroll = self.assistant.scroll.min(max);
        let top = r.y - (max - self.assistant.scroll);
        let turn = (self.assistant.started.map_or(0, |t| t.elapsed().as_millis() / 120) % 8) as u32;
        for (i, (off, h, lines)) in blocks.into_iter().enumerate() {
            let y0 = top + off;
            if y0 > r.bottom() || y0 + h < r.y {
                continue;
            }
            let x = r.x + PAD;
            match &entries[i] {
                Entry::User(_) => {
                    let bubble = Rect::new(x, y0, w, h);
                    c.bordered(bubble, self.color("input.background"), self.color("widget.border"), 1.0, 8.0);
                    draw_lines(c, x + 10.0, y0 + 8.0, &lines);
                }
                Entry::Agent(_) => {
                    // Code lines get the code block background.
                    let mut y = y0;
                    for (line, st) in &lines {
                        let lh = st.line_height.max(16.0);
                        if st.font == render::Font::Mono {
                            c.fill(Rect::new(x - 4.0, y, w + 8.0, lh), self.color("textCodeBlock.background"));
                        }
                        c.text(x, y, line, st);
                        y += lh;
                    }
                }
                Entry::Thought(_) => {
                    c.fill(Rect::new(x, y0, 2.0, h - 4.0), self.color("textBlockQuote.border"));
                    draw_lines(c, x + 10.0, y0, &lines);
                }
                Entry::Tool(t) => {
                    let (icon, color) = match t.status.as_str() {
                        "completed" => (&icons::CHECK, self.color("testing.iconPassed")),
                        "failed" => (&icons::ERROR, self.color("errorForeground")),
                        "pending" => (&icons::CIRCLE_OUTLINE, dim),
                        _ => (&icons::SYNC, dim),
                    };
                    let card = Rect::new(x, y0, w, h);
                    c.bordered(card, self.color("editorWidget.background"), self.color("widget.border"), 1.0, 8.0);
                    if t.status == "in_progress" {
                        c.icon_turned(icon, x + 8.0, y0 + 6.0, 14.0, color, turn);
                    } else {
                        c.icon(icon, x + 8.0, y0 + 6.0, 14.0, color);
                    }
                    draw_lines(c, x + 28.0, y0 + 4.0, &lines);
                    let diffs = t.content.iter().filter(|c| matches!(c, ToolContent::Diff(_))).count();
                    if diffs > 0 {
                        let b = Rect::new(x + 28.0, y0 + h - 28.0, 70.0, 22.0);
                        self.small_button(c, b, "Review", Hit::AssistantReview(i, 0));
                    }
                }
                Entry::Plan(p) => {
                    let mut y = y0 + 4.0;
                    for (e, (line, st)) in p.iter().zip(&lines) {
                        let (icon, color) = match e.status.as_str() {
                            "completed" => (&icons::CHECK, self.color("testing.iconPassed")),
                            "in_progress" => (&icons::ARROW_RIGHT, self.color("focusBorder")),
                            _ => (&icons::CIRCLE_OUTLINE, dim),
                        };
                        c.icon(icon, x, y + 1.0, 14.0, color);
                        c.text(x + 20.0, y, line, st);
                        y += st.line_height.max(16.0);
                    }
                }
                Entry::Permission(p) => {
                    let card = Rect::new(x, y0, w, h);
                    let border = if p.answer.is_none() { self.color("focusBorder") } else { self.color("widget.border") };
                    c.bordered(card, self.color("editorWidget.background"), border, 1.0, 8.0);
                    let mut y = draw_lines(c, x + 10.0, y0 + 8.0, &lines) + 6.0;
                    if !p.diffs.is_empty() {
                        for d in 0..p.diffs.len().min(1) {
                            self.small_button(c, Rect::new(x + 10.0, y, 110.0, 24.0), "Review Changes", Hit::AssistantReview(i, d));
                        }
                        y += 30.0;
                    }
                    match &p.answer {
                        Some(answer) => {
                            c.text(x + 10.0, y + 4.0, &format!("Answered: {answer}"), &TextStyle::ui(SMALL, dim));
                        }
                        None => {
                            let bw = ((w - 30.0) / 2.0).floor();
                            for (k, (_, name, kind)) in p.options.iter().enumerate() {
                                let b = Rect::new(x + 10.0 + (k % 2) as f32 * (bw + 10.0), y + (k / 2) as f32 * 32.0, bw, 26.0);
                                let primary = kind == "allow_once";
                                self.option_button(c, b, name, primary, Hit::AssistantOption(i, k));
                            }
                        }
                    }
                }
                Entry::Auth(methods) => {
                    let card = Rect::new(x, y0, w, h);
                    c.bordered(card, self.color("editorWidget.background"), self.color("focusBorder"), 1.0, 8.0);
                    let y = draw_lines(c, x + 10.0, y0 + 8.0, &lines) + 6.0;
                    for (k, (_, name)) in methods.iter().enumerate() {
                        let b = Rect::new(x + 10.0, y + k as f32 * 32.0, w - 20.0, 26.0);
                        self.option_button(c, b, name, k == 0, Hit::AssistantAuth(i, k));
                    }
                }
                Entry::Notice(_) => {
                    draw_lines(c, x, y0, &lines);
                }
                Entry::Action(_, actions) => {
                    let card = Rect::new(x, y0, w, h);
                    c.bordered(card, self.color("editorWidget.background"), self.color("focusBorder"), 1.0, 8.0);
                    let y = draw_lines(c, x + 10.0, y0 + 8.0, &lines) + 6.0;
                    for (k, (label, _)) in actions.iter().enumerate() {
                        let b = Rect::new(x + 10.0, y + k as f32 * 32.0, w - 20.0, 26.0);
                        self.option_button(c, b, label, k == 0, Hit::AssistantAction(i, k));
                    }
                }
            }
        }
        c.pop_clip();
    }

    fn small_button(&mut self, c: &mut Canvas, b: Rect, label: &str, hit: Hit) {
        let bg = if self.hovered(hit) { self.color("button.secondaryHoverBackground") } else { self.color("button.secondaryBackground") };
        c.fill_rounded(b, bg, 5.0);
        let st = TextStyle::ui(SMALL, self.color("button.secondaryForeground"));
        let lw = c.measure(label, &st);
        c.text_in(Rect::new(b.x + (b.w - lw) / 2.0, b.y, lw + 2.0, b.h), label, &st);
        self.hits.push((b, hit));
    }

    fn option_button(&mut self, c: &mut Canvas, b: Rect, label: &str, primary: bool, hit: Hit) {
        let hovered = self.hovered(hit);
        let (bg, fg) = if primary {
            (if hovered { self.color("button.hoverBackground") } else { self.color("button.background") }, self.color("button.foreground"))
        } else {
            (if hovered { self.color("button.secondaryHoverBackground") } else { self.color("button.secondaryBackground") }, self.color("button.secondaryForeground"))
        };
        c.fill_rounded(b, bg, 6.0);
        let st = TextStyle::ui(UI, fg);
        let lw = c.measure(label, &st).min(b.w - 12.0);
        c.text_fit(Rect::new(b.x + ((b.w - lw) / 2.0).max(6.0), b.y, lw + 2.0, b.h), label, &st);
        self.hits.push((b, hit));
    }

    fn draw_assistant_input(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        c.fill(Rect::new(r.x, r.y, r.w, 1.0), self.color("sideBarSectionHeader.border"));
        let chip_y = r.y + 6.0;
        // New chat at the right, the agent menu before it.
        let new_chat = Rect::new(r.right() - PAD - 22.0, chip_y - 1.0, 22.0, 22.0);
        self.icon_button(c, new_chat, &icons::ADD, Hit::AssistantNewChat, dim);
        let agent = self.agent_choice().label();
        let st = TextStyle::ui(SMALL, dim);
        let aw = (c.measure(&agent, &st) + 26.0).min(r.w * 0.5);
        let menu = Rect::new(new_chat.x - 6.0 - aw, chip_y, aw, 20.0);
        if self.hovered(Hit::AssistantAgentMenu) {
            c.fill_rounded(menu, self.color("toolbar.hoverBackground"), 5.0);
        }
        c.text_fit(Rect::new(menu.x + 6.0, menu.y, menu.w - 24.0, menu.h), &agent, &st);
        c.icon(&icons::CHEVRON_DOWN, menu.right() - 17.0, menu.y + 4.0, 12.0, dim);
        self.assistant.menu_at = (menu.x, menu.bottom() + 2.0);
        self.hits.push((menu, Hit::AssistantAgentMenu));
        // Context chips: the active file (click to leave it out of the next message).
        let file = self.active_doc().and_then(|d| d.buffer.path()).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let x = r.x + PAD;
        let room = menu.x - 8.0 - x;
        if let Some(file) = file.filter(|_| self.settings.bool("assistant.sendActiveFile") && room > 50.0) {
            let on = self.assistant.send_file;
            let st = TextStyle::ui(SMALL, if on { self.color("textLink.foreground") } else { dim });
            let sel = self.active_editor().is_some_and(|e| !e.sel.is_empty());
            let label = if sel { format!("{file} · selection") } else { file };
            let cw = c.measure(&label, &st) + 30.0;
            let chip = Rect::new(x, chip_y, cw.min(room), 20.0);
            c.bordered(chip, Color::TRANSPARENT, if on { self.color("focusBorder").with_alpha(0.5) } else { self.color("widget.border") }, 1.0, 10.0);
            c.icon(&icons::FILE, chip.x + 7.0, chip.y + 3.0, 13.0, if on { self.color("textLink.foreground") } else { dim });
            c.text_fit(Rect::new(chip.x + 24.0, chip.y, chip.w - 28.0, chip.h), &label, &st);
            self.hits.push((chip, Hit::AssistantChip));
        }

        let field = Rect::new(r.x + PAD, r.bottom() - INPUT_H - 8.0, r.w - 2.0 * PAD - 36.0, INPUT_H);
        let focused = self.focus == Focus::Assistant;
        let border = if focused { self.color("focusBorder") } else { self.color("input.border") };
        c.bordered(field, self.color("input.background"), border, 1.0, 8.0);
        let st = TextStyle::ui(UI, fg);
        let placeholder = match self.assistant.phase {
            Phase::Working => "Working… (Esc stops)",
            Phase::Starting => "Starting the agent…",
            _ => "Ask, or describe a change",
        };
        let caret_on = self.caret_on();
        let sel_bg = self.color("editor.selectionBackground");
        self.assistant.input.draw(c, Rect::new(field.x + 10.0, field.y, field.w - 14.0, field.h), &st, placeholder, self.color("input.placeholderForeground"), focused, caret_on, sel_bg);
        self.hits.push((field, Hit::AssistantInput));
        let button = Rect::new(field.right() + 6.0, field.y + 1.0, 28.0, 28.0);
        if self.assistant.phase == Phase::Working {
            let turn = (self.assistant.started.map_or(0, |t| t.elapsed().as_millis() / 120) % 8) as u32;
            let _ = turn;
            self.icon_button(c, button, &icons::DEBUG_STOP, Hit::AssistantStop, self.color("errorForeground"));
        } else {
            let can_send = !self.assistant.input.text.trim().is_empty();
            if can_send {
                c.fill_rounded(button, self.color("button.background"), 7.0);
            }
            c.icon_in(&icons::ARROW_UP, button, 16.0, if can_send { self.color("button.foreground") } else { dim });
            self.hits.push((button, Hit::AssistantSend));
        }
    }
}

/// Draws `lines` from (x, y); returns where they end.
fn draw_lines(c: &mut Canvas, x: f32, mut y: f32, lines: &[(String, TextStyle)]) -> f32 {
    for (line, st) in lines {
        c.text(x, y, line, st);
        y += st.line_height.max(16.0);
    }
    y
}

/// "main.rs  +3 −1" for a diff (a new file: "+N").
fn diff_summary(d: &update::Diff) -> String {
    let name = Path::new(&d.path).file_name().map_or_else(|| d.path.clone(), |n| n.to_string_lossy().into_owned());
    let old = d.old_text.as_deref().unwrap_or("");
    let (mut added, mut removed) = (0, 0);
    for row in scm::side_by_side(old, &d.new_text) {
        match row {
            scm::DiffRow::Equal { .. } => {}
            scm::DiffRow::Changed { .. } => {
                removed += 1;
                added += 1;
            }
            scm::DiffRow::Removed { .. } => removed += 1,
            scm::DiffRow::Added { .. } => added += 1,
        }
    }
    if d.old_text.is_none() { format!("{name}  (new, +{added})") } else { format!("{name}  +{added} −{removed}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_lines() {
        let t = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(t, None, None), t);
        assert_eq!(slice_lines(t, Some(2), Some(2)), "b\nc\n");
        assert_eq!(slice_lines(t, Some(3), None), "c\nd\n");
        assert_eq!(slice_lines(t, None, Some(1)), "a\n");
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    fn draw(wb: &mut Workbench, r: &mut Option<render::Renderer>) {
        if let Some(r) = r {
            let bg = wb.background();
            r.frame(bg, |c| wb.draw(c));
        } else {
            wb.assistant_tick();
        }
    }

    /// Ticks until `done` holds (or fails after a while).
    fn until(wb: &mut Workbench, r: &mut Option<render::Renderer>, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}: {:#?}", wb.assistant.entries);
            std::thread::sleep(Duration::from_millis(20));
            draw(wb, r);
        }
    }

    fn type_and_send(wb: &mut Workbench, text: &str) {
        wb.run(crate::commands::Command::AssistantFocus);
        wb.assistant.input.set_text(text);
        wb.assistant_send();
    }

    /// A conversation with `testdata/fake_acp.py`: streamed text, a plan, a tool call that
    /// asks first, and the edit it makes going into the open document (saved, since it had no
    /// unsaved changes).
    #[test]
    fn a_conversation_with_a_fake_agent() {
        let dir = std::env::temp_dir().join(format!("orbvane-assistant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".orbvane")).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let agent = dir.join("fake_acp.py");
        std::fs::write(&agent, include_str!("../../testdata/fake_acp.py")).unwrap();
        std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let command = serde_json::to_string(&agent.to_string_lossy()).unwrap();
        std::fs::write(dir.join(".orbvane/settings.json"), format!("{{ \"assistant.agent\": \"custom\", \"assistant.agent.command\": {command} }}")).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[file.clone()], std::sync::Arc::new(|| {}));
        let mut r = render::Renderer::offscreen((1200, 760), 1.0).ok();
        draw(&mut wb, &mut r);
        assert!(!wb.aux.visible);

        type_and_send(&mut wb, "make it polite");
        assert!(wb.aux.visible && wb.focus == Focus::Assistant);
        assert_eq!(wb.assistant.entries[0], Entry::User("make it polite".into()));
        until(&mut wb, &mut r, "the permission request", |wb| wb.assistant.entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        let (i, p) = wb.assistant.entries.iter().enumerate().find_map(|(i, e)| if let Entry::Permission(p) = e { Some((i, p.clone())) } else { None }).unwrap();
        // The tool call's diff comes with the question.
        assert_eq!(p.diffs.len(), 1);
        assert_eq!(p.diffs[0].new_text, "goodbye world\n");
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "You said: make it polite")));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Thought(_))));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2)));
        if r.is_some() {
            // The question's buttons are drawn, and Review opens the change in a diff tab.
            assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantOption(i, 0)));
            wb.assistant_review(i, 0);
            assert!(wb.active_editor().is_some_and(|e| e.diff.is_some()));
            let g = wb.active_group;
            let active = wb.groups[g].active;
            wb.close_tab(g, active);
            draw(&mut wb, &mut r);
        }
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the end of the answer", |wb| wb.assistant.phase == Phase::Ready);
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.status == "completed")));
        assert!(matches!(&wb.assistant.entries[i], Entry::Permission(p) if p.answer.as_deref() == Some("Allow")));
        // The open document changed, and was saved.
        let doc = wb.docs.iter().flatten().find(|d| d.buffer.path() == Some(file.as_path())).unwrap();
        assert_eq!(doc.buffer.text(), "goodbye world\n");
        assert!(!doc.buffer.is_dirty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world\n");
        if let (Some(out), Some(r)) = (std::env::var_os("ORBVANE_ASSISTANT_SNAPSHOT"), &r) {
            let (w, h, px) = r.pixels();
            render::write_png(std::path::Path::new(&out), w, h, &px).unwrap();
        }

        // Rejected: nothing changes.
        type_and_send(&mut wb, "again");
        until(&mut wb, &mut r, "the second question", |wb| wb.assistant.entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == 2);
        let j = wb.assistant.entries.iter().rposition(|e| matches!(e, Entry::Permission(_))).unwrap();
        wb.assistant_answer(j, 1);
        until(&mut wb, &mut r, "the end of the second answer", |wb| wb.assistant.phase == Phase::Ready);
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.status == "failed")));

        // Stopping while it asks answers the question "cancelled".
        type_and_send(&mut wb, "once more");
        until(&mut wb, &mut r, "the third question", |wb| wb.assistant.entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == 3);
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.phase == Phase::Ready);
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));

        // New Chat: an empty transcript and a new session.
        wb.run(crate::commands::Command::AssistantNewChat);
        assert!(wb.assistant.entries.is_empty());
        until(&mut wb, &mut r, "the new session", |wb| wb.assistant.phase == Phase::Ready);

        // An agent that wants a sign-in first: its methods are buttons, picking one goes on.
        // (Without the file, so the agent just answers.)
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "sign in next time");
        until(&mut wb, &mut r, "the answer", |wb| wb.assistant.phase == Phase::Ready && wb.assistant.entries.len() > 2);
        assert!(!wb.assistant.entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        wb.run(crate::commands::Command::AssistantNewChat);
        until(&mut wb, &mut r, "the sign-in", |wb| wb.assistant.phase == Phase::SigningIn);
        let k = wb.assistant.entries.iter().position(|e| matches!(e, Entry::Auth(m) if m[0].1 == "Use a token")).unwrap();
        if r.is_some() {
            let (rect, _) = *wb.hits.iter().find(|(_, h)| *h == Hit::AssistantAuth(k, 0)).unwrap();
            wb.mouse_down(rect.x + 4.0, rect.y + 4.0, false, false, false);
            wb.mouse_up();
        } else {
            wb.assistant_authenticate("token");
        }
        until(&mut wb, &mut r, "the session after signing in", |wb| wb.assistant.phase == Phase::Ready);
        assert!(!wb.assistant.entries.iter().any(|e| matches!(e, Entry::Auth(_))));

        wb.shutdown();
        assert!(wb.assistant.client.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn workbench(name: &str, settings: &str) -> (Workbench, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbvane-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".orbvane")).unwrap();
        std::fs::write(dir.join(".orbvane/settings.json"), settings).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        (Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {})), dir)
    }

    /// No agent yet: the setup screen lists the agents, with the button their tools call for.
    #[test]
    fn the_setup_screen_offers_the_agents() {
        let (mut wb, dir) = workbench("setup", "{}");
        let Ok(mut r) = render::Renderer::offscreen((1200, 760), 1.0) else { return };
        let claude = agents::find("claude-code").unwrap();
        let codex = agents::find("codex").unwrap();
        wb.assistant.probes.insert(claude.id, Probe { path: Some("/x/claude".into()), version: "2.1".into(), signed_in: true });
        wb.assistant.probes.insert(codex.id, Probe::default());
        wb.run(crate::commands::Command::AssistantFocus);
        r.frame(wb.background(), |c| wb.draw(c));
        for i in 0..agents::AGENTS.len() {
            assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantAgent(i as u8)));
        }
        assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantCustomAgent));
        assert_eq!(wb.agent_setup_action(claude), AgentAction::Use("claude-code"));
        assert_eq!(wb.agent_setup_action(codex), AgentAction::Install("codex"));
        // A tool that's installed but signed out needs a sign-in, which Use says.
        wb.assistant.probes.insert(codex.id, Probe { path: Some("/x/codex".into()), version: "1".into(), signed_in: false });
        assert_eq!(wb.agent_setup_action(codex), AgentAction::SignIn("codex"));
        // (Saved in the workspace: the user settings are shared by the tests.)
        wb.use_agent("codex", settings::Scope::Workspace);
        assert_eq!(wb.agent_choice(), Choice::Builtin(codex));
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Action(_, a)) if a[0].1 == AgentAction::SignIn("codex")));
        // The picker lists both agents and a custom command.
        wb.run(crate::commands::Command::AssistantSelectAgent);
        let labels: Vec<String> = wb.palette.as_ref().unwrap().picker.as_ref().unwrap().choices.iter().map(|i| i.label.clone()).collect();
        assert_eq!(labels, ["Claude Code", "Codex (current)", "Custom Command…"]);
        wb.palette = None;
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex through the bridge, with `testdata/fake_codex.py` standing in for `codex
    /// app-server`: unsaved changes are saved first, the reply streams with the model the
    /// account offers (the configured one isn't), the file change is a permission question
    /// with its diff and lands on disk when allowed, a failed turn shows Codex's message, and
    /// Stop interrupts the turn.
    #[test]
    fn a_conversation_with_codex() {
        let (mut wb, dir) = workbench("codex", r#"{ "assistant.agent": "codex" }"#);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let fake = dir.join("fake_codex.py");
        std::fs::write(&fake, include_str!("../../testdata/fake_codex.py")).unwrap();
        wb.assistant.server_override = Some(format!("exec python3 {}", fake.display()));
        wb.assistant.probes.insert("codex", Probe { path: Some("/x/codex".into()), version: "1".into(), signed_in: true });
        wb.open_file(&file);
        let id = wb.active_editor().unwrap().doc;
        let end = wb.docs[id].as_ref().unwrap().buffer.pos_of(11);
        wb.edit_doc(id, vec![(end, end, "!".into())], false);
        assert!(wb.docs[id].as_ref().unwrap().buffer.is_dirty());
        let mut r = None;

        type_and_send(&mut wb, "make it polite");
        // Saved first, since Codex reads the file from disk.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world!\n");
        until(&mut wb, &mut r, "the permission request", |wb| wb.assistant.entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        let (i, p) = wb.assistant.entries.iter().enumerate().find_map(|(i, e)| if let Entry::Permission(p) = e { Some((i, p.clone())) } else { None }).unwrap();
        assert_eq!(p.title, "Edit notes.txt");
        assert_eq!(p.diffs.len(), 1);
        assert_eq!(p.diffs[0].new_text, "goodbye world!\n");
        assert_eq!(p.options.iter().map(|o| o.1.as_str()).collect::<Vec<_>>(), ["Allow", "Allow These Files for This Chat", "Reject"]);
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "Using model=m1")), "{:#?}", wb.assistant.entries);
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Thought(t) if t.contains("retired-model") && t.contains("Model One"))));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2 && p[1].status == "in_progress")));
        assert_eq!(wb.assistant.agent_name, "Codex");
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the end of the turn", |wb| wb.assistant.phase == Phase::Ready);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world!\n");
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Run `ls -1`" && t.status == "completed")));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "completed")));
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Agent(t)) if t == "Done."), "{:#?}", wb.assistant.entries.last());
        // The editor tools went to Codex's config.
        assert!(wb.output.lines("Assistant").iter().any(|l| l == "mcp servers: ['orbvane']"), "{:?}", wb.output.lines("Assistant"));

        // A failed turn: Codex's own message.
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "fail now");
        until(&mut wb, &mut r, "the failure", |wb| wb.assistant.phase == Phase::Ready && matches!(wb.assistant.entries.last(), Some(Entry::Notice(_))));
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Notice(n)) if n == "The agent reported an error: Model is unavailable."));

        // Stop interrupts the turn.
        type_and_send(&mut wb, "wait for me");
        until(&mut wb, &mut r, "the turn to start", |wb| wb.assistant.phase == Phase::Working);
        std::thread::sleep(Duration::from_millis(200));
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.phase == Phase::Ready);
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Claude Code through the bridge, with `testdata/fake_claude.py` standing in for
    /// `claude`: the flags that make it ask us, the stream (thinking, text, the plan), an edit
    /// asked for with its diff and landing on disk, "allow for this chat" remembered for a
    /// command, a rejected edit, an expired sign-in offering Sign In, and Stop.
    #[test]
    fn a_conversation_with_claude_code() {
        let (mut wb, dir) = workbench("claude", r#"{ "assistant.agent": "claude-code" }"#);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let fake = dir.join("fake_claude.py");
        std::fs::write(&fake, include_str!("../../testdata/fake_claude.py")).unwrap();
        wb.assistant.server_override = Some(format!("exec python3 {} \"$@\"", fake.display()));
        wb.assistant.probes.insert("claude-code", Probe { path: Some("/x/claude".into()), version: "2.1".into(), signed_in: true });
        wb.open_file(&file);
        let mut r = None;
        let question = |wb: &Workbench, n: usize| wb.assistant.entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == n;
        let last_question = |wb: &Workbench| wb.assistant.entries.iter().rposition(|e| matches!(e, Entry::Permission(_))).unwrap();

        type_and_send(&mut wb, "make it polite");
        until(&mut wb, &mut r, "the edit question", |wb| question(wb, 1));
        let i = last_question(&wb);
        let Entry::Permission(p) = &wb.assistant.entries[i] else { unreachable!() };
        assert_eq!(p.title, "Edit notes.txt");
        assert_eq!(p.diffs[0].new_text, "goodbye world\n");
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "Using tools")), "{:#?}", wb.assistant.entries);
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Thought(t) if t == "Looking at it.")));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2 && p[1].status == "in_progress")));
        assert_eq!(wb.assistant.agent_name, "Claude Code");
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the command question", |wb| question(wb, 2));
        let j = last_question(&wb);
        let Entry::Permission(p) = &wb.assistant.entries[j] else { unreachable!() };
        assert_eq!(p.title, "Run `ls -1`");
        assert_eq!(p.options[1].1, "Allow This Command for This Chat");
        wb.assistant_answer(j, 1);
        until(&mut wb, &mut r, "the end of the answer", |wb| wb.assistant.phase == Phase::Ready);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world\n");
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "completed")));
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Run `ls -1`" && t.status == "completed")));
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Agent(t)) if t == "Done."), "{:#?}", wb.assistant.entries.last());
        let log = wb.output.lines("Assistant");
        assert!(log.iter().any(|l| l == "flags ok") && log.iter().any(|l| l == "mcp servers: ['orbvane']"), "{log:?}");

        // Again: the edit is asked for (rejected this time), the command isn't.
        std::fs::write(&file, "hello again\n").unwrap();
        type_and_send(&mut wb, "once more");
        until(&mut wb, &mut r, "the second edit question", |wb| question(wb, 3));
        let k = last_question(&wb);
        wb.assistant_answer(k, 2);
        until(&mut wb, &mut r, "the end of the second answer", |wb| wb.assistant.phase == Phase::Ready);
        assert!(question(&wb, 3), "the command was asked about again");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello again\n");
        assert!(wb.assistant.entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "failed")));

        // An expired sign-in: a Sign In button.
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "expired?");
        until(&mut wb, &mut r, "the sign-in prompt", |wb| wb.assistant.phase == Phase::Ready && matches!(wb.assistant.entries.last(), Some(Entry::Action(..))));
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Action(t, a)) if t.contains("sign-in has expired") && a[0].1 == AgentAction::SignIn("claude-code")));

        // Stop interrupts it.
        type_and_send(&mut wb, "wait for me");
        until(&mut wb, &mut r, "the turn to start", |wb| wb.assistant.phase == Phase::Working);
        std::thread::sleep(Duration::from_millis(200));
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.phase == Phase::Ready);
        assert!(matches!(wb.assistant.entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real agent named by `ORBVANE_REAL_AGENT` (claude-code or codex), with its real tool:
    /// one small prompt in a scratch folder, every question answered Allow. Uses the account,
    /// so it only runs when asked: `ORBVANE_REAL_AGENT=codex cargo test -p app real_agent -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_agent() {
        let Ok(id) = std::env::var("ORBVANE_REAL_AGENT") else { return };
        let (mut wb, dir) = workbench(&format!("real-{id}"), &format!(r#"{{ "assistant.agent": "{id}" }}"#));
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        wb.open_file(&file);
        type_and_send(&mut wb, "In notes.txt, change hello to goodbye. Then run `ls -1`. Reply with one short sentence.");
        let start = Instant::now();
        let mut shown = 0;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            wb.assistant_tick();
            let entries = wb.assistant.entries.clone();
            for (i, e) in entries.iter().enumerate() {
                if let Entry::Permission(p) = e {
                    if p.answer.is_none() {
                        eprintln!("asked: {} ({:?}), diffs: {:?}", p.title, p.options.iter().map(|o| &o.1).collect::<Vec<_>>(), p.diffs.iter().map(|d| &d.new_text).collect::<Vec<_>>());
                        wb.assistant_answer(i, 0);
                    }
                }
            }
            for e in &wb.assistant.entries[shown.min(wb.assistant.entries.len())..] {
                eprintln!("entry: {e:?}");
            }
            shown = wb.assistant.entries.len();
            let done = wb.assistant.phase == Phase::Ready || (wb.assistant.client.is_none() && start.elapsed() > Duration::from_secs(5));
            if done || start.elapsed() > Duration::from_secs(240) {
                break;
            }
        }
        eprintln!("final entries: {:#?}", wb.assistant.entries);
        eprintln!("output: {:#?}", wb.output.lines("Assistant"));
        eprintln!("file: {:?}", std::fs::read_to_string(&file));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A custom command that isn't an agent (it exits at once): the transcript says so, quotes
    /// what it printed and offers the agents.
    #[test]
    fn a_command_that_is_not_an_agent() {
        let (mut wb, dir) = workbench("not-an-agent", r#"{ "assistant.agent": "custom", "assistant.agent.command": "sh -c 'echo usage: no terminal >&2'" }"#);
        assert_eq!(wb.agent_choice().label(), "sh");
        let mut r = None;
        type_and_send(&mut wb, "hello");
        until(&mut wb, &mut r, "the agent's exit", |wb| wb.assistant.client.is_none() && wb.assistant.entries.iter().any(|e| matches!(e, Entry::Action(..))));
        let Some(Entry::Action(text, actions)) = wb.assistant.entries.last() else { panic!("{:?}", wb.assistant.entries) };
        assert!(text.contains("doesn't seem to speak the Agent Client Protocol"), "{text}");
        assert!(text.contains("usage: no terminal"), "{text}");
        assert_eq!(actions.last().map(|a| &a.1), Some(&AgentAction::Choose));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
