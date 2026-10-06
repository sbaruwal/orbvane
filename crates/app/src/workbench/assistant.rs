//! The Assistant: chats with coding agents in the secondary sidebar. Each chat talks to one
//! agent: one of the agents Orbvane knows (`crate::agents`; new chats get the one
//! `assistant.agent` names) or any program that speaks the Agent Client Protocol (`crate acp`),
//! started from `assistant.agent.command`. Every chat runs its own agent, so several can work
//! at once; an agent that sits idle stops and continues its conversation (`session/load`) with
//! the next message. Chats are kept per folder or workspace (`chat_store.rs`) until deleted:
//! the open ones are tabs above the transcript, the rest are in the History.
//!
//! The agent reads and writes files through the editor (open documents included, unsaved
//! changes and all), asks before acting (`session/request_permission`: the options are buttons
//! in the transcript), and with `assistant.editorTools` it can ask our language servers about
//! the code (`mcp.rs`).
//!
//! Without an agent the Assistant shows the known ones with what their tools need (installed,
//! signed in: `agents::probe`), and installing or signing in runs in a task terminal.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acp::update::{self, ToolContent, Update};
use acp::{Client, Incoming};
use render::{Canvas, Color, Rect, TextStyle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::chat_store::{self, Meta, Saved};
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
/// The chat tabs' row.
const STRIP_H: f32 = 32.0;
const HISTORY_ROW: f32 = 44.0;
/// The chat's header: its agent, permission mode and model.
const HEADER_H: f32 = 28.0;
/// How many of the agent's last stderr lines an error quotes.
const RECENT_LOG: usize = 4;
/// An agent with nothing to do for this long stops (its chat continues where it was).
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);

/// Something to do about the agent or a chat: from the setup screen, a menu, a picker or a
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
    /// A new chat with this agent (`Choice::key`; empty: the default one).
    NewChat(String),
    /// A chat's menu, by its id.
    RenameChat(String),
    CloseChat(String),
    DeleteChat(String),
    /// The shown chat's permission mode or model (ids the agent offers).
    SetMode(String),
    SetModel(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Tool {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub content: Vec<ToolContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A message with buttons: what to do about the agent (never saved).
    #[serde(skip)]
    Action(String, Vec<(String, AgentAction)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// No agent running.
    Off,
    /// Started; waiting for `initialize` and `session/new` (or `session/load`).
    Starting,
    /// Waiting for the user to sign in (an `Entry::Auth`).
    SigningIn,
    Ready,
    /// A prompt is being answered.
    Working,
}

/// What a chat's tab shows about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatStatus {
    Idle,
    Working,
    /// A question or a sign-in waits for the user.
    NeedsYou,
    /// It finished while another chat was shown.
    Unread,
}

/// One conversation with one agent.
pub(super) struct Chat {
    pub id: String,
    /// What its tab and the History call it: the first message, unless renamed.
    pub title: String,
    /// Its agent (`Choice::key`); empty: the one the settings name.
    pub agent: String,
    /// Unix seconds.
    created: u64,
    updated: u64,
    /// The agent's session: the running one, or the one to continue.
    session: Option<String>,
    pub entries: Vec<Entry>,
    pub input: TextField,
    /// Pixels scrolled up from the bottom of the transcript.
    pub scroll: f32,
    /// The transcript's height in the last frame.
    content_h: f32,
    client: Option<Client>,
    pub phase: Phase,
    init_req: Option<i64>,
    session_req: Option<i64>,
    prompt_req: Option<i64>,
    auth_req: Option<i64>,
    /// `session_req` continues `session`: the updates meanwhile replay what we already have.
    loading: bool,
    /// Whether the agent takes file contents in prompts (`embeddedContext`).
    embedded: bool,
    pub agent_name: String,
    /// The sign-in methods the agent offers.
    auth_methods: Vec<(String, String)>,
    /// A prompt waiting for the session to start.
    queued: Option<Vec<Value>>,
    started: Option<Instant>,
    /// The folder the agent was started in.
    cwd: Option<PathBuf>,
    /// The agent's last lines on stderr.
    recent_log: VecDeque<String>,
    pub unread: bool,
    /// When the agent last did something (idle agents stop).
    last_active: Instant,
    /// The permission mode and model chosen for it (ids the agent offers; None: the default),
    /// applied whenever its agent starts.
    pub mode: Option<String>,
    pub model: Option<String>,
    /// What the running agent offers, (id, name, description) and (id, name), and uses now.
    modes: Vec<(String, String, String)>,
    models: Vec<(String, String)>,
    pub cur_mode: Option<String>,
    cur_model: Option<String>,
    /// `session/set_mode` and `session/set_model` waiting: (request, the id asked for).
    mode_req: Option<(i64, String)>,
    model_req: Option<(i64, String)>,
}

impl Chat {
    /// The agent's name as it introduced itself, else "The agent".
    fn agent_name_or_default(&self) -> &str {
        if self.agent_name.is_empty() { "The agent" } else { &self.agent_name }
    }

    fn new(agent: String) -> Self {
        let now = unix_now();
        Self {
            id: acp::uuid(),
            title: String::new(),
            agent,
            created: now,
            updated: now,
            session: None,
            entries: Vec::new(),
            input: TextField::default(),
            scroll: 0.0,
            content_h: 0.0,
            client: None,
            phase: Phase::Off,
            init_req: None,
            session_req: None,
            prompt_req: None,
            auth_req: None,
            loading: false,
            embedded: false,
            agent_name: String::new(),
            auth_methods: Vec::new(),
            queued: None,
            started: None,
            cwd: None,
            recent_log: VecDeque::new(),
            unread: false,
            last_active: Instant::now(),
            mode: None,
            model: None,
            modes: Vec::new(),
            models: Vec::new(),
            cur_mode: None,
            cur_model: None,
            mode_req: None,
            model_req: None,
        }
    }

    fn from_saved(s: Saved) -> Self {
        let mut chat = Self::new(s.meta.agent);
        chat.id = s.meta.id;
        chat.title = s.meta.title;
        chat.created = s.meta.created;
        chat.updated = s.meta.updated;
        chat.session = s.session;
        chat.mode = s.mode;
        chat.model = s.model;
        chat.entries = s.entries;
        chat
    }

    /// Whether anything was said in it (only those are kept).
    pub fn has_messages(&self) -> bool {
        self.entries.iter().any(|e| matches!(e, Entry::User(_)))
    }

    /// Whether it can still take another agent: nothing running, no conversation to continue.
    fn unstarted(&self) -> bool {
        self.client.is_none() && self.session.is_none()
    }

    pub fn label(&self) -> &str {
        if self.title.is_empty() { "New Chat" } else { &self.title }
    }

    pub fn status(&self) -> ChatStatus {
        let asks = self.entries.iter().any(|e| matches!(e, Entry::Permission(p) if p.answer.is_none()) || matches!(e, Entry::Auth(_)));
        match self.phase {
            _ if asks || self.phase == Phase::SigningIn => ChatStatus::NeedsYou,
            Phase::Working | Phase::Starting => ChatStatus::Working,
            _ if self.unread => ChatStatus::Unread,
            _ => ChatStatus::Idle,
        }
    }

    /// What's written to disk: no buttons, and questions that can't be answered any more
    /// marked so.
    fn saved(&self) -> Saved {
        let entries = self
            .entries
            .iter()
            .filter(|e| !matches!(e, Entry::Action(..) | Entry::Auth(_)))
            .cloned()
            .map(|mut e| {
                match &mut e {
                    Entry::Permission(p) if p.answer.is_none() => p.answer = Some("Not answered".into()),
                    Entry::Tool(t) if t.status == "pending" || t.status == "in_progress" => t.status = "interrupted".into(),
                    _ => {}
                }
                e
            })
            .collect();
        let meta = Meta { id: self.id.clone(), title: self.label().to_string(), agent: self.agent.clone(), created: self.created, updated: self.updated };
        Saved { meta, session: self.session.clone(), mode: self.mode.clone(), model: self.model.clone(), entries }
    }

    /// Stops the agent; the session stays, to continue.
    fn stop(&mut self) {
        if let Some(mut c) = self.client.take() {
            c.shutdown();
        }
        self.phase = Phase::Off;
        self.prompt_req = None;
        self.session_req = None;
        self.init_req = None;
        self.auth_req = None;
        self.loading = false;
        self.mode_req = None;
        self.model_req = None;
        self.cur_mode = None;
        self.cur_model = None;
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Notice(text.into()));
        self.scroll = 0.0;
    }
}

pub(super) struct Assistant {
    /// The open chats (tabs), never empty, and the one shown.
    pub chats: Vec<Chat>,
    pub active: usize,
    /// The chats kept for this folder (the History), newest first.
    pub saved: Vec<Meta>,
    /// Where they're kept (`chat_store::dir_for` the workspace).
    dir: Option<PathBuf>,
    /// The History shows instead of the chat.
    pub history: bool,
    history_scroll: f32,
    history_h: f32,
    /// Where the transcript (or the History) was drawn.
    pub body: Rect,
    /// Whether the active file goes with the next message (the chip toggles it).
    pub send_file: bool,
    /// What the checks found about the known agents' tools, the ones being checked, and where
    /// the checks answer.
    pub probes: HashMap<&'static str, Probe>,
    probing: HashSet<&'static str>,
    probe_tx: Sender<(&'static str, Probe)>,
    probe_rx: Receiver<(&'static str, Probe)>,
    /// Install and sign-in terminals running: task label → agent id.
    tasks: HashMap<String, &'static str>,
    /// Where the agent menu and the new chat menu open (below their buttons).
    menu_at: (f32, f32),
    new_menu_at: (f32, f32),
    mode_menu_at: (f32, f32),
    model_menu_at: (f32, f32),
    /// Tests: the shell command that stands in for a built-in agent's tool (`codex app-server`).
    pub server_override: Option<String>,
}

impl Default for Assistant {
    fn default() -> Self {
        let (probe_tx, probe_rx) = mpsc::channel();
        Self {
            chats: vec![Chat::new(String::new())],
            active: 0,
            saved: Vec::new(),
            dir: None,
            history: false,
            history_scroll: 0.0,
            history_h: 0.0,
            body: Rect::default(),
            send_file: true,
            probes: HashMap::new(),
            probing: HashSet::new(),
            probe_tx,
            probe_rx,
            tasks: HashMap::new(),
            menu_at: (0.0, 0.0),
            new_menu_at: (0.0, 0.0),
            mode_menu_at: (0.0, 0.0),
            model_menu_at: (0.0, 0.0),
            server_override: None,
        }
    }
}

impl Assistant {
    /// The chat shown.
    pub fn cur(&self) -> &Chat {
        &self.chats[self.active]
    }

    pub fn cur_mut(&mut self) -> &mut Chat {
        &mut self.chats[self.active]
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A chat's title from its first message: the first line, shortened.
fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(59).collect::<String>().trim_end())
    } else {
        line.to_string()
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
    /// The agent the settings name (new chats get it).
    pub(super) fn agent_choice(&self) -> Choice {
        agents::choice(&self.settings.string("assistant.agent"), &self.settings.string("assistant.agent.command"))
    }

    /// The agent chat `ci` talks to.
    pub(super) fn chat_choice(&self, ci: usize) -> Choice {
        match self.assistant.chats[ci].agent.as_str() {
            "" => self.agent_choice(),
            key => agents::from_key(key),
        }
    }

    /// The folder the agent works in.
    fn agent_cwd(&self) -> PathBuf {
        self.folder().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()))
    }

    fn chat_notice(&mut self, ci: usize, text: impl Into<String>) {
        self.assistant.chats[ci].notice(text);
    }

    /// A message with buttons in chat `ci`'s transcript.
    fn chat_action(&mut self, ci: usize, text: String, actions: Vec<(String, AgentAction)>) {
        let chat = &mut self.assistant.chats[ci];
        chat.entries.push(Entry::Action(text, actions));
        chat.scroll = 0.0;
    }

    fn chat_choose_prompt(&mut self, ci: usize, text: &str) {
        self.chat_action(ci, text.into(), vec![("Choose Agent".into(), AgentAction::Choose)]);
    }

    /// Starts chat `ci`'s agent (if it has one and it isn't running).
    fn chat_start(&mut self, ci: usize) {
        if self.assistant.chats[ci].client.is_some() {
            return;
        }
        match self.chat_choice(ci) {
            Choice::None => {
                self.assistant.chats[ci].queued = None;
                self.chat_choose_prompt(ci, "Choose the agent to talk to first.");
            }
            Choice::Custom(command) => self.start_custom_agent(ci, &command),
            Choice::Builtin(a) => self.start_builtin_agent(ci, a),
        }
    }

    /// One of our agents: its tool has to be installed and signed in.
    fn start_builtin_agent(&mut self, ci: usize, a: &'static agents::Agent) {
        if !self.agent_ready(ci, a) {
            self.assistant.chats[ci].queued = None;
            return;
        }
        match a.id {
            "codex" => self.start_codex(ci),
            _ => self.start_claude(ci),
        }
    }

    /// Claude Code through our bridge (`acp::claude`), which runs `claude -p` with JSON in and
    /// out. The bridge adds its own arguments after these ("$@").
    fn start_claude(&mut self, ci: usize) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let command = self.assistant.server_override.clone().unwrap_or_else(|| "exec claude \"$@\"".into());
        let cwd = self.agent_cwd();
        let options = acp::Options { program: shell, args: vec!["-l".into(), "-c".into(), command, "claude".into()], cwd: cwd.clone(), env: self.mcp_env() };
        let version = self.assistant.probes.get("claude-code").map(|p| p.version.clone()).unwrap_or_default();
        match Client::in_process("claude", self.waker.clone(), move |rx, tx, log| acp::claude::serve(options, version, rx, tx, log)) {
            Ok(client) => self.agent_started(ci, client, Choice::Builtin(agents::find("claude-code").unwrap()), cwd),
            Err(e) => self.chat_notice(ci, format!("Couldn't start Claude Code: {e}")),
        }
    }

    /// Codex through our bridge (`acp::codex`), which runs `codex app-server`.
    fn start_codex(&mut self, ci: usize) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let command = self.assistant.server_override.clone().unwrap_or_else(|| "exec codex app-server".into());
        let cwd = self.agent_cwd();
        let options = acp::Options { program: shell, args: vec!["-l".into(), "-c".into(), command], cwd: cwd.clone(), env: self.mcp_env() };
        match Client::in_process("codex", self.waker.clone(), move |rx, tx, log| acp::codex::serve(options, rx, tx, log)) {
            Ok(client) => self.agent_started(ci, client, Choice::Builtin(agents::find("codex").unwrap()), cwd),
            Err(e) => self.chat_notice(ci, format!("Couldn't start Codex: {e}")),
        }
    }

    /// An agent is running: it gets `initialize`.
    fn agent_started(&mut self, ci: usize, mut client: Client, choice: Choice, cwd: PathBuf) {
        let chat = &mut self.assistant.chats[ci];
        chat.init_req = Some(client.initialize("orbvane", env!("CARGO_PKG_VERSION")));
        chat.client = Some(client);
        chat.phase = Phase::Starting;
        chat.started = Some(Instant::now());
        chat.last_active = Instant::now();
        chat.cwd = Some(cwd);
        chat.agent = choice.key();
        chat.recent_log.clear();
        let title = chat.label().to_string();
        self.output.append(OUTPUT_CHANNEL, &format!("Starting {} for \"{title}\"\n", choice.label()));
    }

    /// Whether `a`'s tool is installed and signed in, as far as the last check knows; if
    /// not, chat `ci` says what to do (a check still running counts as ready).
    fn agent_ready(&mut self, ci: usize, a: &'static agents::Agent) -> bool {
        let Some(p) = self.assistant.probes.get(a.id).cloned() else {
            self.assistant_probe(a);
            return true;
        };
        if p.path.is_none() {
            let text = format!("{} needs its command line tool, `{}`, which isn't installed.", a.name, a.program);
            self.chat_action(ci, text, vec![(format!("Install {}", a.name), AgentAction::Install(a.id)), ("Choose Another Agent".into(), AgentAction::Choose)]);
            return false;
        }
        if !p.signed_in {
            let text = format!("{} needs you to sign in first.", a.name);
            self.chat_action(ci, text, vec![("Sign In".into(), AgentAction::SignIn(a.id)), ("Choose Another Agent".into(), AgentAction::Choose)]);
            return false;
        }
        true
    }

    /// An Agent Client Protocol agent started with `command` through the login shell.
    fn start_custom_agent(&mut self, ci: usize, command: &str) {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        // The login shell finds the agent the way a terminal would; exec so it gets the signals.
        let args = vec!["-l".to_string(), "-c".to_string(), format!("exec {command}")];
        let cwd = self.agent_cwd();
        let env = self.mcp_env();
        match Client::spawn(&shell, &args, &cwd, &env, self.waker.clone()) {
            Ok(client) => self.agent_started(ci, client, Choice::Custom(command.to_string()), cwd),
            Err(e) => self.chat_notice(ci, format!("Couldn't start the agent ({command}): {e}")),
        }
    }

    // ------------------------------------------------------------------ keeping chats

    /// Where this folder's (or workspace's) chats are kept.
    fn chats_dir(&self) -> PathBuf {
        self.assistant.dir.clone().unwrap_or_else(|| chat_store::dir_for(self.workspace_id().as_deref()))
    }

    /// Writes chat `ci` (if anything was said in it).
    fn write_chat(&self, ci: usize) {
        let chat = &self.assistant.chats[ci];
        if chat.has_messages() {
            chat_store::save(&self.chats_dir(), &chat.saved());
        }
    }

    fn save_chat(&mut self, ci: usize) {
        self.write_chat(ci);
        self.assistant.saved = chat_store::list(&self.chats_dir());
    }

    /// The open chats' ids (the ones kept) and the shown one's, for the session.
    pub(super) fn assistant_session(&self) -> (Vec<String>, Option<String>) {
        let a = &self.assistant;
        (a.chats.iter().filter(|c| c.has_messages()).map(|c| c.id.clone()).collect(), Some(a.cur().id.clone()))
    }

    /// Another folder opened: its chats replace these (saved first). In the same folders, agents
    /// started in a folder that's gone stop.
    pub(super) fn assistant_folders_changed(&mut self) {
        let dir = chat_store::dir_for(self.workspace_id().as_deref());
        if self.assistant.dir.as_ref() == Some(&dir) {
            let cwd = self.agent_cwd();
            for chat in &mut self.assistant.chats {
                if chat.client.is_some() && chat.cwd.as_ref() != Some(&cwd) {
                    chat.stop();
                }
            }
            return;
        }
        if self.assistant.dir.is_some() {
            self.assistant_shutdown();
        }
        let a = &mut self.assistant;
        a.chats = vec![Chat::new(String::new())];
        a.active = 0;
        a.history = false;
        a.saved = chat_store::list(&dir);
        a.dir = Some(dir);
    }

    /// Reopens the chats the session had open (`ids`), showing `active`.
    pub(super) fn assistant_restore(&mut self, ids: &[String], active: Option<&str>) {
        self.assistant_folders_changed();
        if self.assistant.chats.iter().any(|c| c.has_messages() || c.client.is_some()) {
            return;
        }
        let dir = self.chats_dir();
        let chats: Vec<Chat> = ids.iter().filter_map(|id| chat_store::load(&dir, id)).map(Chat::from_saved).collect();
        if chats.is_empty() {
            return;
        }
        self.assistant.active = active.and_then(|id| chats.iter().position(|c| c.id == id)).unwrap_or(0);
        self.assistant.chats = chats;
    }

    /// Saves every chat and stops their agents (quitting, or another folder).
    pub(super) fn assistant_shutdown(&mut self) {
        for ci in 0..self.assistant.chats.len() {
            self.write_chat(ci);
            self.assistant.chats[ci].stop();
        }
    }

    fn chat_index(&self, id: &str) -> Option<usize> {
        self.assistant.chats.iter().position(|c| c.id == id)
    }

    /// Shows chat `ci`.
    pub(super) fn select_chat(&mut self, ci: usize) {
        if ci < self.assistant.chats.len() {
            self.assistant.active = ci;
            self.assistant.history = false;
            self.assistant.chats[ci].unread = false;
        }
    }

    /// New Chat (with `agent`, a `Choice::key`, or the default one): an empty chat that's
    /// already open is reused.
    pub(super) fn assistant_new_chat(&mut self, agent: String) {
        let a = &mut self.assistant;
        let ci = match a.chats.iter().position(|c| !c.has_messages() && c.unstarted()) {
            Some(i) => i,
            None => {
                a.chats.push(Chat::new(String::new()));
                a.chats.len() - 1
            }
        };
        a.chats[ci].agent = agent;
        a.chats[ci].entries.clear();
        self.select_chat(ci);
        self.show_aux(super::aux_bar::AuxTab::Assistant);
        self.focus = Focus::Assistant;
        if let Choice::Builtin(a) = self.chat_choice(ci) {
            if !self.assistant.probes.contains_key(a.id) {
                self.assistant_probe(a);
            }
        }
    }

    /// Closes chat `ci`'s tab (it stays in the History) and stops its agent.
    pub(super) fn close_chat(&mut self, ci: usize) {
        if ci >= self.assistant.chats.len() {
            return;
        }
        self.save_chat(ci);
        let a = &mut self.assistant;
        a.chats[ci].stop();
        a.chats.remove(ci);
        if a.chats.is_empty() {
            a.chats.push(Chat::new(String::new()));
        }
        if ci < a.active || a.active >= a.chats.len() {
            a.active = a.active.saturating_sub(1);
        }
        let active = a.active;
        self.select_chat(active);
        self.assistant.history = false;
    }

    /// Deletes a chat for good (after asking).
    fn delete_chat(&mut self, id: &str) {
        let title = self.chat_index(id).map(|i| self.assistant.chats[i].label().to_string()).or_else(|| self.assistant.saved.iter().find(|m| m.id == id).map(|m| m.title.clone()));
        let Some(title) = title else { return };
        if !cfg!(test) {
            let yes = self
                .message_dialog()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(format!("Delete the chat \"{title}\"?"))
                .set_description("Its transcript is removed from this project. This can't be undone.")
                .set_buttons(rfd::MessageButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
                .show()
                == rfd::MessageDialogResult::Custom("Delete".into());
            if !yes {
                return;
            }
        }
        let history = self.assistant.history;
        if let Some(ci) = self.chat_index(id) {
            // Emptied first, so closing doesn't save it again.
            self.assistant.chats[ci].entries.clear();
            self.close_chat(ci);
        }
        let dir = self.chats_dir();
        chat_store::delete(&dir, id);
        self.assistant.saved = chat_store::list(&dir);
        self.assistant.history = history;
    }

    /// Opens History row `i` (or shows it, if it's open).
    pub(super) fn open_saved_chat(&mut self, i: usize) {
        let Some(meta) = self.assistant.saved.get(i).cloned() else { return };
        let ci = match self.chat_index(&meta.id) {
            Some(ci) => ci,
            None => {
                let Some(saved) = chat_store::load(&self.chats_dir(), &meta.id) else { return };
                self.assistant.chats.push(Chat::from_saved(saved));
                // An empty chat that was open gives way.
                let empty = self.assistant.chats.iter().position(|c| !c.has_messages() && c.unstarted());
                if let Some(e) = empty {
                    self.assistant.chats.remove(e);
                }
                self.assistant.chats.len() - 1
            }
        };
        self.select_chat(ci);
        self.focus = Focus::Assistant;
    }

    /// The input box named a chat.
    pub(super) fn chat_named(&mut self, id: &str, title: String) {
        let Some(ci) = self.chat_index(id) else { return };
        let title = title.trim();
        if !title.is_empty() {
            self.assistant.chats[ci].title = title.chars().take(80).collect();
            self.save_chat(ci);
        }
    }

    /// Shows or hides the History.
    pub(super) fn assistant_toggle_history(&mut self) {
        let a = &mut self.assistant;
        a.history = !a.history;
        a.history_scroll = 0.0;
        if a.history {
            self.assistant.saved = chat_store::list(&self.chats_dir());
        }
    }

    /// A chat tab's menu.
    pub(super) fn chat_menu(&mut self, ci: usize, x: f32, y: f32) {
        let Some(chat) = self.assistant.chats.get(ci) else { return };
        let id = chat.id.clone();
        let kept = chat.has_messages();
        let item = |label: &str, enabled: bool| PopupItem::Item { label: label.into(), enabled, checked: None };
        let entries = vec![
            (item("Rename…", kept), PopupAction::Agent(AgentAction::RenameChat(id.clone()))),
            (item("Close", true), PopupAction::Agent(AgentAction::CloseChat(id.clone()))),
            (PopupItem::Separator, PopupAction::None),
            (item("Delete…", kept), PopupAction::Agent(AgentAction::DeleteChat(id))),
        ];
        self.show_popup(entries, x, y);
    }

    /// The new chat menu: one entry per agent.
    pub(super) fn assistant_new_chat_menu(&mut self) {
        self.assistant_probe_all();
        let item = |label: String| PopupItem::Item { label, enabled: true, checked: None };
        let mut entries: Vec<(PopupItem, PopupAction)> = agents::AGENTS.iter().map(|a| (item(format!("New {} Chat", a.name)), PopupAction::Agent(AgentAction::NewChat(a.id.into())))).collect();
        let command = self.settings.string("assistant.agent.command");
        if !command.trim().is_empty() {
            let custom = Choice::Custom(command.trim().to_string());
            entries.push((item(format!("New {} Chat", custom.label())), PopupAction::Agent(AgentAction::NewChat(custom.key()))));
        }
        entries.push((PopupItem::Separator, PopupAction::None));
        entries.push((item("Custom Command…".into()), PopupAction::Agent(AgentAction::Custom)));
        let (x, y) = self.assistant.new_menu_at;
        self.show_popup(entries, x, y);
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
                self.open_input("The command that starts an agent that speaks the Agent Client Protocol.", "Agent command", purpose, current.trim());
            }
            AgentAction::Choose => self.assistant_select_agent(),
            AgentAction::NewChat(agent) => self.assistant_new_chat(agent),
            AgentAction::RenameChat(id) => {
                let Some(ci) = self.chat_index(&id) else { return };
                let title = self.assistant.chats[ci].label().to_string();
                self.open_input("A name for this chat.", "Chat name", super::GitInput::ChatName(id), &title);
            }
            AgentAction::CloseChat(id) => {
                if let Some(ci) = self.chat_index(&id) {
                    self.close_chat(ci);
                }
            }
            AgentAction::DeleteChat(id) => self.delete_chat(&id),
            AgentAction::SetMode(mode) => self.set_chat_mode(mode),
            AgentAction::SetModel(model) => self.set_chat_model(model),
        }
    }

    // ------------------------------------------------------------------ modes and models

    /// The permission modes chat `ci` offers: the running agent's, else what ours offer.
    fn chat_modes(&self, ci: usize) -> Vec<(String, String, String)> {
        let chat = &self.assistant.chats[ci];
        if !chat.modes.is_empty() {
            return chat.modes.clone();
        }
        let own = |v: &[(&str, &str, &str)]| v.iter().map(|(id, name, about)| (id.to_string(), name.to_string(), about.to_string())).collect();
        match self.chat_choice(ci) {
            Choice::Builtin(a) if a.id == "codex" => own(&acp::codex::MODES),
            Choice::Builtin(_) => own(&acp::claude::MODES.map(|(id, _, name, about)| (id, name, about))),
            _ => Vec::new(),
        }
    }

    /// The mode chat `ci` is in (or will start in), if its agent has modes.
    fn chat_mode(&self, ci: usize) -> Option<(String, String, String)> {
        let chat = &self.assistant.chats[ci];
        let modes = self.chat_modes(ci);
        let want = chat.mode.clone().or_else(|| chat.cur_mode.clone()).unwrap_or_else(|| self.settings.string("assistant.permissions"));
        modes.iter().find(|m| m.0 == want).or_else(|| chat.cur_mode.as_ref().and_then(|c| modes.iter().find(|m| &m.0 == c))).cloned()
    }

    /// The agent just started a session: it gets the chat's mode (else the default one) and
    /// model, if they're not what it uses.
    fn apply_chat_settings(&mut self, ci: usize) {
        let default = self.settings.string("assistant.permissions");
        let chat = &mut self.assistant.chats[ci];
        let want = chat.mode.clone().unwrap_or(default);
        if chat.modes.iter().any(|m| m.0 == want) {
            chat.mode = Some(want.clone());
            if chat.cur_mode.as_deref() != Some(want.as_str()) {
                if let (Some(c), Some(session)) = (&mut chat.client, &chat.session) {
                    chat.mode_req = Some((c.request("session/set_mode", json!({ "sessionId": session, "modeId": want })), want));
                }
            }
        }
        if let Some(model) = chat.model.clone().filter(|m| chat.models.iter().any(|x| &x.0 == m) && chat.cur_model.as_deref() != Some(m.as_str())) {
            if let (Some(c), Some(session)) = (&mut chat.client, &chat.session) {
                chat.model_req = Some((c.request("session/set_model", json!({ "sessionId": session, "modelId": model })), model));
            }
        }
    }

    /// Switches the shown chat's permission mode (now, if its agent runs; else when it starts).
    /// Full Access asks first.
    fn set_chat_mode(&mut self, mode: String) {
        let ci = self.assistant.active;
        if mode == "full" && self.chat_mode(ci).is_none_or(|m| m.0 != "full") && !cfg!(test) {
            let agent = self.chat_choice(ci).label();
            let yes = self
                .message_dialog()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(format!("Give {agent} full access in this chat?"))
                .set_description("It will edit files and run commands without asking, outside its sandbox. Use it only where that's safe.")
                .set_buttons(rfd::MessageButtons::OkCancelCustom("Allow Full Access".into(), "Cancel".into()))
                .show()
                == rfd::MessageDialogResult::Custom("Allow Full Access".into());
            if !yes {
                return;
            }
        }
        let chat = &mut self.assistant.chats[ci];
        chat.mode = Some(mode.clone());
        if chat.cur_mode.as_deref() != Some(mode.as_str()) && chat.session_req.is_none() {
            if let (Some(c), Some(session)) = (&mut chat.client, &chat.session) {
                chat.mode_req = Some((c.request("session/set_mode", json!({ "sessionId": session, "modeId": mode })), mode));
            }
        }
        self.save_chat(ci);
    }

    fn set_chat_model(&mut self, model: String) {
        let ci = self.assistant.active;
        let chat = &mut self.assistant.chats[ci];
        chat.model = Some(model.clone());
        if chat.cur_model.as_deref() != Some(model.as_str()) && chat.session_req.is_none() {
            if let (Some(c), Some(session)) = (&mut chat.client, &chat.session) {
                chat.model_req = Some((c.request("session/set_model", json!({ "sessionId": session, "modelId": model })), model));
            }
        }
        self.save_chat(ci);
    }

    /// The header's permission mode menu.
    pub(super) fn assistant_mode_menu(&mut self) {
        let ci = self.assistant.active;
        let current = self.chat_mode(ci).map(|m| m.0);
        let entries = self
            .chat_modes(ci)
            .into_iter()
            .map(|(id, name, about)| {
                let label = if about.is_empty() { name } else { format!("{name}: {about}") };
                (PopupItem::Item { label, enabled: true, checked: Some(current.as_deref() == Some(id.as_str())) }, PopupAction::Agent(AgentAction::SetMode(id)))
            })
            .collect();
        let (x, y) = self.assistant.mode_menu_at;
        self.show_popup(entries, x, y);
    }

    /// The header's model menu (the agent's models are known once it runs).
    pub(super) fn assistant_model_menu(&mut self) {
        let chat = self.assistant.cur();
        let current = chat.model.clone().or_else(|| chat.cur_model.clone());
        let entries: Vec<(PopupItem, PopupAction)> = if chat.models.is_empty() {
            vec![(PopupItem::Item { label: "The models show once the agent has started".into(), enabled: false, checked: None }, PopupAction::None)]
        } else {
            chat.models.iter().map(|(id, name)| (PopupItem::Item { label: name.clone(), enabled: true, checked: Some(current.as_deref() == Some(id.as_str())) }, PopupAction::Agent(AgentAction::SetModel(id.clone())))).collect()
        };
        let (x, y) = self.assistant.model_menu_at;
        self.show_popup(entries, x, y);
    }

    /// The chat a newly chosen agent is for: the shown one if it hasn't started a conversation
    /// (it follows the settings), else a new one (the shown one keeps its agent).
    fn chat_for_new_agent(&mut self) -> usize {
        let a = &mut self.assistant;
        if a.cur().unstarted() {
            let chat = a.cur_mut();
            chat.agent.clear();
            chat.entries.retain(|e| !matches!(e, Entry::Action(..)));
            return a.active;
        }
        self.assistant_new_chat(String::new());
        self.assistant.active
    }

    /// Makes `id` the agent (saved in `scope`), and says what it still needs.
    fn use_agent(&mut self, id: &'static str, scope: settings::Scope) {
        let Some(a) = agents::find(id) else { return };
        self.update_setting(scope, "assistant.agent", Some(Value::String(id.into())));
        self.apply_settings();
        let ci = self.chat_for_new_agent();
        self.show_aux(super::aux_bar::AuxTab::Assistant);
        self.focus = Focus::Assistant;
        if self.assistant.probes.contains_key(id) {
            self.agent_ready(ci, a);
        } else {
            self.assistant_probe(a);
        }
    }

    /// A custom command was typed.
    pub(super) fn set_custom_agent(&mut self, command: String) {
        self.update_setting(settings::Scope::User, "assistant.agent.command", Some(Value::String(command)));
        self.update_setting(settings::Scope::User, "assistant.agent", Some(Value::String("custom".into())));
        self.apply_settings();
        self.chat_for_new_agent();
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
            Err(e) => self.assistant.cur_mut().notice(e),
        }
    }

    /// A task ended: if it was an install or sign-in, the agent is checked again. Returns
    /// whether it was one.
    pub(super) fn assistant_task_done(&mut self, label: &str, code: Option<i32>) -> bool {
        let Some(id) = self.assistant.tasks.remove(label) else { return false };
        let Some(a) = agents::find(id) else { return true };
        if code != Some(0) {
            self.assistant.cur_mut().notice(format!("{label} didn't finish. The terminal shows what happened."));
        }
        self.assistant.probes.remove(id);
        for chat in &mut self.assistant.chats {
            chat.entries.retain(|e| !matches!(e, Entry::Action(..)));
        }
        self.assistant_probe(a);
        true
    }

    /// Assistant: Select Agent: the agents in the palette.
    pub(super) fn assistant_select_agent(&mut self) {
        self.assistant_probe_all();
        let current = self.chat_choice(self.assistant.active);
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

    /// The agent menu below the message box: the shown chat's agent (picking another one in
    /// a conversation starts a new chat).
    pub(super) fn assistant_agent_menu(&mut self) {
        self.assistant_probe_all();
        let current = self.chat_choice(self.assistant.active);
        let mut entries: Vec<(PopupItem, PopupAction)> = agents::AGENTS
            .iter()
            .map(|a| (PopupItem::Item { label: a.name.into(), enabled: true, checked: Some(current == Choice::Builtin(a)) }, PopupAction::Agent(AgentAction::Use(a.id))))
            .collect();
        entries.push((PopupItem::Separator, PopupAction::None));
        let custom = matches!(current, Choice::Custom(_));
        entries.push((PopupItem::Item { label: "Custom Command…".into(), enabled: true, checked: Some(custom) }, PopupAction::Agent(AgentAction::Custom)));
        // Signing in only when the last check found it signed out; the menu checks again for
        // next time (a sign-in made elsewhere shows up then).
        if let Choice::Builtin(a) = current {
            if self.assistant.probes.get(a.id).is_some_and(|p| p.path.is_some() && !p.signed_in) {
                entries.push((PopupItem::Separator, PopupAction::None));
                entries.push((PopupItem::Item { label: format!("Sign In to {}…", a.name), enabled: true, checked: None }, PopupAction::Agent(AgentAction::SignIn(a.id))));
            }
            if !self.assistant.probing.contains(a.id) {
                self.assistant.probes.remove(a.id);
                self.assistant_probe(a);
            }
        }
        let (x, y) = self.assistant.menu_at;
        self.show_popup(entries, x, y);
    }

    fn new_session(&mut self, ci: usize) {
        let cwd = self.agent_cwd();
        let servers = self.mcp_servers();
        let meta = self.session_meta(&servers);
        let chat = &mut self.assistant.chats[ci];
        if let Some(c) = &mut chat.client {
            chat.session_req = Some(c.request("session/new", json!({ "cwd": cwd, "mcpServers": servers, "_meta": meta })));
        }
    }

    /// What our own agents learn with a session: whether to use the editor's file tools.
    fn session_meta(&self, servers: &[Value]) -> Value {
        json!({ "orbvane.editorFiles": !servers.is_empty() && self.settings.bool("assistant.editorFiles") })
    }

    /// Continues the chat's earlier session with a new agent process.
    fn load_session(&mut self, ci: usize, session: String) {
        let cwd = self.agent_cwd();
        let servers = self.mcp_servers();
        let meta = self.session_meta(&servers);
        let chat = &mut self.assistant.chats[ci];
        if let Some(c) = &mut chat.client {
            chat.session_req = Some(c.request("session/load", json!({ "sessionId": session, "cwd": cwd, "mcpServers": servers, "_meta": meta })));
            chat.loading = true;
        }
    }

    /// Stops the shown chat's answer; open permission questions are answered "cancelled".
    pub(super) fn assistant_cancel(&mut self) {
        let chat = self.assistant.cur_mut();
        if chat.phase != Phase::Working {
            return;
        }
        let Some(c) = &mut chat.client else { return };
        if let Some(session) = &chat.session {
            c.notify("session/cancel", json!({ "sessionId": session }));
        }
        for e in &mut chat.entries {
            if let Entry::Permission(p) = e {
                if p.answer.is_none() {
                    c.respond(p.request.clone(), json!({ "outcome": { "outcome": "cancelled" } }));
                    p.answer = Some("Cancelled".into());
                }
            }
        }
    }

    /// Sends what's typed in the shown chat, with the active file and selection as context.
    pub(super) fn assistant_send(&mut self) {
        let ci = self.assistant.active;
        let text = self.assistant.chats[ci].input.text.trim().to_string();
        if text.is_empty() || self.assistant.chats[ci].phase == Phase::Working {
            return;
        }
        let choice = self.chat_choice(ci);
        if choice == Choice::None {
            self.chat_choose_prompt(ci, "Choose the agent to talk to first.");
            return;
        }
        if matches!(choice, Choice::Builtin(_)) && self.settings.bool("assistant.saveBeforeSending") {
            self.save_for_agent();
        }
        let prompt = self.assistant_prompt(ci, &text);
        let chat = &mut self.assistant.chats[ci];
        chat.input.set_text("");
        if chat.title.is_empty() {
            chat.title = title_from(&text);
        }
        chat.entries.push(Entry::User(text));
        chat.scroll = 0.0;
        chat.updated = unix_now();
        chat.last_active = Instant::now();
        if chat.phase == Phase::Ready {
            self.send_prompt(ci, prompt);
        } else {
            chat.queued = Some(prompt);
            self.chat_start(ci);
        }
        self.save_chat(ci);
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
    fn assistant_prompt(&self, ci: usize, text: &str) -> Vec<Value> {
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
            if self.assistant.chats[ci].embedded {
                blocks.push(json!({ "type": "text", "text": note }));
                blocks.push(json!({ "type": "resource", "resource": { "uri": format!("{uri}#L{}-{}", a.line + 1, b.line + 1), "text": selected, "mimeType": "text/plain" } }));
            } else {
                blocks.push(json!({ "type": "text", "text": format!("{note}\n```\n{selected}\n```") }));
            }
        }
        blocks.push(json!({ "type": "resource_link", "uri": uri, "name": name }));
        blocks
    }

    fn send_prompt(&mut self, ci: usize, prompt: Vec<Value>) {
        let chat = &mut self.assistant.chats[ci];
        let (Some(c), Some(session)) = (&mut chat.client, &chat.session) else { return };
        chat.prompt_req = Some(c.request("session/prompt", json!({ "sessionId": session, "prompt": prompt })));
        chat.phase = Phase::Working;
    }

    /// Handles what the agents sent. Called every frame.
    pub(super) fn assistant_tick(&mut self) {
        while let Ok((id, probe)) = self.assistant.probe_rx.try_recv() {
            self.assistant.probing.remove(id);
            self.assistant.probes.insert(id, probe);
            // Checked after Use or an install: say what's still missing.
            let ci = self.assistant.active;
            if let (Choice::Builtin(a), true) = (self.chat_choice(ci), self.assistant.cur().client.is_none()) {
                let entries = &self.assistant.cur().entries;
                if a.id == id && !entries.iter().any(|e| matches!(e, Entry::Action(..))) && !entries.is_empty() {
                    self.agent_ready(ci, a);
                }
            }
        }
        for ci in 0..self.assistant.chats.len() {
            self.chat_tick(ci);
        }
        // Idle agents stop; their chats continue with the next message.
        for chat in &mut self.assistant.chats {
            if chat.client.is_some() && chat.phase == Phase::Ready && chat.status() == ChatStatus::Idle && chat.last_active.elapsed() >= IDLE_STOP {
                chat.stop();
                let title = chat.label().to_string();
                self.output.append(OUTPUT_CHANNEL, &format!("Stopped the idle agent of \"{title}\"\n"));
            }
        }
    }

    fn chat_tick(&mut self, ci: usize) {
        let Some(client) = self.assistant.chats[ci].client.as_mut() else { return };
        let messages = client.poll();
        if !messages.is_empty() {
            self.assistant.chats[ci].last_active = Instant::now();
        }
        for msg in messages {
            match msg {
                Incoming::Response { id, result } => self.chat_response(ci, id, result),
                Incoming::Notification { method, params } if method == "session/update" => {
                    let chat = &self.assistant.chats[ci];
                    if !chat.loading && params["sessionId"].as_str() == chat.session.as_deref() {
                        self.chat_update(ci, update::parse(&params["update"]));
                    }
                }
                Incoming::Notification { .. } => {}
                Incoming::Request { id, method, params } => self.chat_request(ci, id, &method, &params),
                Incoming::Log(line) => {
                    self.output.append(OUTPUT_CHANNEL, &format!("{line}\n"));
                    let recent = &mut self.assistant.chats[ci].recent_log;
                    if !line.trim().is_empty() {
                        recent.push_back(line);
                        if recent.len() > RECENT_LOG {
                            recent.pop_front();
                        }
                    }
                }
                Incoming::Exited => {
                    let chat = &mut self.assistant.chats[ci];
                    let starting = chat.phase == Phase::Starting && chat.init_req.is_some();
                    chat.stop();
                    let choice = self.chat_choice(ci);
                    if starting && matches!(choice, Choice::Custom(_)) {
                        self.agent_exited_starting(ci);
                    } else if starting {
                        let printed: Vec<String> = self.assistant.chats[ci].recent_log.iter().cloned().collect();
                        let mut text = format!("{} stopped while starting.", choice.label());
                        if !printed.is_empty() {
                            text.push_str(&format!(" It printed:\n{}", printed.join("\n")));
                        }
                        self.chat_notice(ci, text);
                    } else {
                        self.chat_notice(ci, "The agent exited. Send a message to start it again.");
                    }
                    return;
                }
            }
        }
    }

    /// A custom agent ended before answering `initialize`: most likely a program that doesn't
    /// speak the protocol (an agent's interactive tool), so say so and offer the agents we know.
    fn agent_exited_starting(&mut self, ci: usize) {
        let command = match self.chat_choice(ci) {
            Choice::Custom(c) => c,
            _ => String::new(),
        };
        let program = Choice::Custom(command.clone()).label();
        let mut text = format!("`{program}` stopped before answering, so it doesn't seem to speak the Agent Client Protocol.");
        let printed: Vec<String> = self.assistant.chats[ci].recent_log.iter().cloned().collect();
        if !printed.is_empty() {
            text.push_str(&format!(" It printed:\n{}", printed.join("\n")));
        }
        let mut actions = Vec::new();
        if let Some(a) = agents::AGENTS.iter().find(|a| a.program == program) {
            text.push_str(&format!("\n{} is built in: use it instead.", a.name));
            actions.push((format!("Use {}", a.name), AgentAction::Use(a.id)));
        }
        actions.push(("Choose Agent".into(), AgentAction::Choose));
        self.chat_action(ci, text, actions);
    }

    /// The sign-in an agent of ours asked for: a button that signs in.
    fn chat_sign_in(&mut self, ci: usize, message: String) -> bool {
        let Choice::Builtin(agent) = self.chat_choice(ci) else { return false };
        self.assistant.probes.remove(agent.id);
        self.chat_action(ci, message, vec![("Sign In".into(), AgentAction::SignIn(agent.id))]);
        true
    }

    fn chat_response(&mut self, ci: usize, id: i64, result: Result<Value, (i64, String)>) {
        let chat = &mut self.assistant.chats[ci];
        if Some(id) == chat.init_req {
            chat.init_req = None;
            match result {
                Ok(r) => {
                    chat.embedded = r["agentCapabilities"]["promptCapabilities"]["embeddedContext"].as_bool() == Some(true);
                    chat.agent_name = r["agentInfo"]["title"].as_str().or(r["agentInfo"]["name"].as_str()).unwrap_or("").to_string();
                    let methods: Vec<(String, String)> = r["authMethods"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|m| Some((m["id"].as_str()?.to_string(), m["name"].as_str()?.to_string())))
                        .collect();
                    // Kept for when session/new asks for a sign-in.
                    chat.entries.retain(|e| !matches!(e, Entry::Auth(_)));
                    chat.auth_methods = methods;
                    let can_load = r["agentCapabilities"]["loadSession"].as_bool() == Some(true);
                    match chat.session.clone() {
                        Some(session) if can_load => self.load_session(ci, session),
                        Some(_) => {
                            chat.session = None;
                            chat.notice("This agent can't continue an earlier conversation, so this is a new one: it doesn't know what was said above.");
                            self.new_session(ci);
                        }
                        None => self.new_session(ci),
                    }
                }
                Err((_, e)) => {
                    chat.stop();
                    chat.notice(format!("The agent didn't start: {e}"));
                }
            }
        } else if Some(id) == chat.session_req {
            chat.session_req = None;
            let loading = std::mem::take(&mut chat.loading);
            match result {
                Ok(r) => {
                    // A loaded session keeps its id (the answer has none).
                    if !loading {
                        chat.session = r["sessionId"].as_str().map(String::from);
                    }
                    chat.phase = Phase::Ready;
                    (chat.cur_mode, chat.modes) = update::modes(&r);
                    (chat.cur_model, chat.models) = update::models(&r);
                    // The chat's mode and model before its message.
                    self.apply_chat_settings(ci);
                    let chat = &mut self.assistant.chats[ci];
                    if let Some(prompt) = chat.queued.take() {
                        self.send_prompt(ci, prompt);
                    }
                    self.save_chat(ci);
                }
                Err((_, e)) if loading => {
                    chat.session = None;
                    chat.notice(format!("The agent couldn't continue the earlier conversation ({e}), so this is a new one: it doesn't know what was said above."));
                    self.new_session(ci);
                }
                Err((code, _)) if code == AUTH_REQUIRED && !chat.auth_methods.is_empty() => {
                    chat.phase = Phase::SigningIn;
                    let methods = chat.auth_methods.clone();
                    chat.entries.push(Entry::Auth(methods));
                }
                Err((code, e)) => {
                    chat.phase = Phase::Ready;
                    chat.queued = None;
                    if !(code == AUTH_REQUIRED && self.chat_sign_in(ci, e.clone())) {
                        self.chat_notice(ci, format!("The agent couldn't start a session: {e}"));
                    }
                }
            }
        } else if chat.mode_req.as_ref().is_some_and(|r| r.0 == id) {
            let (_, mode) = chat.mode_req.take().unwrap();
            match result {
                Ok(_) => chat.cur_mode = Some(mode),
                Err((_, e)) => {
                    chat.mode = chat.cur_mode.clone();
                    let name = chat.modes.iter().find(|m| m.0 == mode).map_or(mode.clone(), |m| m.1.clone());
                    chat.notice(format!("Couldn't switch to {name}: {e}"));
                }
            }
            self.save_chat(ci);
        } else if chat.model_req.as_ref().is_some_and(|r| r.0 == id) {
            let (_, model) = chat.model_req.take().unwrap();
            match result {
                Ok(_) => chat.cur_model = Some(model),
                Err((_, e)) => {
                    chat.model = chat.cur_model.clone();
                    chat.notice(format!("Couldn't switch to the model {model}: {e}"));
                }
            }
            self.save_chat(ci);
        } else if Some(id) == chat.auth_req {
            chat.auth_req = None;
            match result {
                Ok(_) => {
                    chat.entries.retain(|e| !matches!(e, Entry::Auth(_)));
                    chat.phase = Phase::Starting;
                    self.new_session(ci);
                }
                Err((_, e)) => {
                    chat.phase = Phase::SigningIn;
                    chat.notice(format!("Signing in didn't work: {e}"));
                }
            }
        } else if Some(id) == chat.prompt_req {
            chat.prompt_req = None;
            chat.phase = Phase::Ready;
            chat.updated = unix_now();
            chat.unread = ci != self.assistant.active;
            // The reply, read out when it's in the chat being shown.
            let reply = chat.entries.iter().rev().take_while(|e| !matches!(e, Entry::User(_))).find_map(|e| match e {
                Entry::Agent(t) => Some(t.clone()),
                _ => None,
            });
            if let Some(reply) = reply.filter(|_| ci == self.assistant.active && result.is_ok()) {
                self.a11y_say(plain_text(&reply));
            }
            let chat = &mut self.assistant.chats[ci];
            match result {
                Ok(r) => match r["stopReason"].as_str().unwrap_or("end_turn") {
                    "end_turn" => {}
                    "cancelled" => chat.notice("Stopped."),
                    "max_tokens" => chat.notice("The agent stopped: it reached its output limit."),
                    "max_turn_requests" => chat.notice("The agent stopped: it reached its limit of steps for one message."),
                    "refusal" => chat.notice("The agent declined to continue."),
                    other => chat.notice(format!("The agent stopped ({other}).")),
                },
                // One of ours whose sign-in expired: the button signs in again.
                Err((code, e)) => {
                    if !(code == AUTH_REQUIRED && self.chat_sign_in(ci, e.clone())) {
                        self.chat_notice(ci, format!("The agent reported an error: {e}"));
                    }
                }
            }
            self.save_chat(ci);
        }
    }

    /// Picks a sign-in method in the shown chat.
    pub(super) fn assistant_authenticate(&mut self, method: &str) {
        let chat = self.assistant.cur_mut();
        if let Some(c) = &mut chat.client {
            chat.auth_req = Some(c.request("authenticate", json!({ "methodId": method })));
        }
    }

    fn chat_update(&mut self, ci: usize, u: Update) {
        let entries = &mut self.assistant.chats[ci].entries;
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
            // The agent switched (after a plan is approved, say).
            Update::Mode(mode) => {
                let chat = &mut self.assistant.chats[ci];
                chat.cur_mode = Some(mode.clone());
                chat.mode = Some(mode);
            }
            Update::UserText(_) | Update::Commands(_) | Update::Other(_) => {}
        }
    }

    fn chat_request(&mut self, ci: usize, id: Value, method: &str, params: &Value) {
        match method {
            "session/request_permission" => {
                let call = update::permission_tool_call(params);
                let chat = &mut self.assistant.chats[ci];
                // The title from the request, else from the tool call it's about.
                let known = chat.entries.iter().rev().find_map(|e| match e {
                    Entry::Tool(t) if t.id == call.id => Some(t.clone()),
                    _ => None,
                });
                let title = call.title.clone().or_else(|| known.as_ref().map(|t| t.title.clone())).unwrap_or_else(|| "The agent wants to continue".into());
                let content = call.content.clone().or_else(|| known.map(|t| t.content)).unwrap_or_default();
                let diffs = content.into_iter().filter_map(|c| if let ToolContent::Diff(d) = c { Some(d) } else { None }).collect();
                let said = format!("{} asks: {title}", chat.agent_name_or_default());
                chat.entries.push(Entry::Permission(Permission { request: id, title, options: update::permission_options(params), answer: None, diffs }));
                chat.scroll = 0.0;
                if ci == self.assistant.active {
                    self.a11y_say(said);
                }
            }
            "fs/read_text_file" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                let text = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path.as_path())).map(|d| d.buffer.text());
                let text = match text {
                    Some(t) => Ok(t),
                    None => std::fs::read_to_string(&path).map_err(|e| e.to_string()),
                };
                let reply = text.map(|t| slice_lines(&t, params["line"].as_u64(), params["limit"].as_u64()));
                match (self.assistant.chats[ci].client.as_mut(), reply) {
                    (Some(c), Ok(t)) => c.respond(id, json!({ "content": t })),
                    (Some(c), Err(e)) => c.respond_error(id, -32002, &format!("{}: {e}", path.display())),
                    _ => {}
                }
            }
            "fs/write_text_file" => {
                let path = PathBuf::from(params["path"].as_str().unwrap_or(""));
                let result = self.agent_write(&path, params["content"].as_str().unwrap_or(""));
                if let Some(c) = &mut self.assistant.chats[ci].client {
                    match result {
                        Ok(()) => c.respond(id, Value::Null),
                        Err(e) => c.respond_error(id, -32603, &format!("{}: {e}", path.display())),
                    }
                }
            }
            _ => {
                if let Some(c) = &mut self.assistant.chats[ci].client {
                    c.respond_error(id, -32601, &format!("{method} isn't supported"));
                }
            }
        }
    }

    /// The agent writes `path`: an open document is edited (one undo step, cursors kept) and
    /// saved unless it had unsaved changes of its own; other files are written to disk.
    pub(super) fn agent_write(&mut self, path: &Path, content: &str) -> Result<(), String> {
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

    /// Answers permission request `entry` of the shown chat with its option `option`.
    pub(super) fn assistant_answer(&mut self, entry: usize, option: usize) {
        let chat = self.assistant.cur_mut();
        let Some(Entry::Permission(p)) = chat.entries.get_mut(entry) else { return };
        if p.answer.is_some() {
            return;
        }
        let Some((id, name, _)) = p.options.get(option).cloned() else { return };
        if let Some(c) = &mut chat.client {
            c.respond(p.request.clone(), json!({ "outcome": { "outcome": "selected", "optionId": id } }));
        }
        p.answer = Some(name);
        chat.last_active = Instant::now();
    }

    /// Shows a proposed change as a diff tab: the file as it is ↔ as the agent would have it.
    pub(super) fn assistant_review(&mut self, entry: usize, diff: usize) {
        let d = match self.assistant.cur().entries.get(entry) {
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
        let chats = &self.assistant.chats;
        // The working spinners turn; idle agents stop.
        let spin = chats.iter().any(|c| c.phase == Phase::Working || c.phase == Phase::Starting).then(|| Instant::now() + Duration::from_millis(120));
        let idle = chats.iter().filter(|c| c.client.is_some() && c.phase == Phase::Ready).map(|c| c.last_active + IDLE_STOP).min();
        spin.into_iter().chain(idle).min()
    }

    pub(super) fn assistant_scroll(&mut self, dy: f32) {
        let a = &mut self.assistant;
        if a.history {
            let max = (a.history_h - a.body.h).max(0.0);
            a.history_scroll = (a.history_scroll - dy).clamp(0.0, max);
            return;
        }
        let body_h = a.body.h;
        let chat = a.cur_mut();
        let max = (chat.content_h - body_h).max(0.0);
        chat.scroll = (chat.scroll + dy).clamp(0.0, max);
    }

    pub(super) fn assistant_key(&mut self, k: &crate::input::KeyInput) {
        use crate::input::Key;
        match k.key {
            Key::Enter => self.assistant_send(),
            Key::Escape if self.assistant.cur().phase == Phase::Working => self.assistant_cancel(),
            Key::Escape => self.focus = Focus::Editor,
            _ => {
                self.assistant.cur_mut().input.key(k);
            }
        }
    }

    pub(super) fn assistant_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let f = &mut self.assistant.cur_mut().input;
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.assistant.cur_mut().input.insert(&text);
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
        let (strip, r) = r.cut_top(STRIP_H);
        self.draw_chat_strip(c, strip, fg, dim);
        if self.assistant.history {
            return self.draw_history(c, r, fg, dim);
        }
        if self.chat_choice(self.assistant.active) == Choice::None && self.assistant.cur().entries.is_empty() {
            return self.draw_assistant_setup(c, r, fg, dim);
        }
        let (header, r) = r.cut_top(HEADER_H);
        self.draw_chat_header(c, header, fg, dim);
        let (body, footer) = r.cut_bottom(INPUT_H + 40.0);
        self.draw_transcript(c, body, fg, dim);
        self.draw_assistant_input(c, footer, fg, dim);
    }

    /// The chat tabs, with the History and New Chat buttons at the right.
    fn draw_chat_strip(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        c.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), self.color("sideBarSectionHeader.border"));
        let add = Rect::new(r.right() - 8.0 - 22.0, r.y + 5.0, 22.0, 22.0);
        self.icon_button(c, add, &icons::ADD, Hit::AssistantNewChat, dim);
        self.assistant.new_menu_at = (add.x, add.bottom() + 2.0);
        let history = Rect::new(add.x - 4.0 - 22.0, add.y, 22.0, 22.0);
        if self.assistant.history {
            c.fill_rounded(history, self.color("toolbar.activeBackground"), 5.0);
        }
        self.icon_button(c, history, &icons::HISTORY, Hit::AssistantHistory, if self.assistant.history { fg } else { dim });
        // The tabs share what's left; when they don't fit, the shown one stays in view.
        let left = r.x + 8.0;
        let room = history.x - 6.0 - left;
        let st = TextStyle::ui(SMALL, fg);
        let n = self.assistant.chats.len();
        let widths: Vec<f32> = self.assistant.chats.iter().map(|ch| (c.measure(ch.label(), &st) + 40.0).clamp(64.0, 160.0)).collect();
        let (widths, first) = if widths.iter().sum::<f32>() + 4.0 * n as f32 <= room {
            (widths, 0)
        } else {
            let w = (room / n as f32 - 4.0).max(64.0);
            let fit = ((room / (w + 4.0)).floor() as usize).max(1);
            let first = (self.assistant.active + 1).saturating_sub(fit);
            (vec![w; n], first)
        };
        let turn = (self.assistant.chats.iter().filter_map(|ch| ch.started).map(|t| t.elapsed().as_millis() / 120).max().unwrap_or(0) % 8) as u32;
        let mut x = left;
        for ci in first..n {
            let w = widths[ci];
            if x + w > left + room + 0.5 {
                break;
            }
            let tab = Rect::new(x, r.y + 5.0, w, 22.0);
            let active = ci == self.assistant.active && !self.assistant.history;
            let hovered = self.hovered(Hit::AssistantTab(ci)) || self.hovered(Hit::AssistantTabClose(ci));
            if active {
                c.fill_rounded(tab, self.color("list.activeSelectionBackground"), 6.0);
            } else if hovered {
                c.fill_rounded(tab, self.color("toolbar.hoverBackground"), 6.0);
            }
            let chat = &self.assistant.chats[ci];
            let mut tx = tab.x + 8.0;
            match chat.status() {
                ChatStatus::Working => {
                    c.icon_turned(&icons::SYNC, tx - 1.0, tab.y + 4.0, 13.0, self.color("focusBorder"), turn);
                    tx += 15.0;
                }
                ChatStatus::NeedsYou => {
                    c.icon(&icons::DOT, tx - 2.0, tab.y + 3.0, 16.0, self.color("editorWarning.foreground"));
                    tx += 14.0;
                }
                ChatStatus::Unread => {
                    c.icon(&icons::DOT, tx - 2.0, tab.y + 3.0, 16.0, self.color("textLink.foreground"));
                    tx += 14.0;
                }
                ChatStatus::Idle => {}
            }
            let label_st = TextStyle::ui(SMALL, if active { fg } else { dim });
            let close_w = if active || hovered { 18.0 } else { 4.0 };
            c.text_fit(Rect::new(tx, tab.y, tab.right() - close_w - tx, tab.h), chat.label(), &label_st);
            self.hits.push((tab, Hit::AssistantTab(ci)));
            let read = chat.label().to_string();
            self.a11y_name(Hit::AssistantTab(ci), super::a11y::Role::Tab, read, active);
            if active || hovered {
                let close = Rect::new(tab.right() - 19.0, tab.y + 3.0, 16.0, 16.0);
                if self.hovered(Hit::AssistantTabClose(ci)) {
                    c.fill_rounded(close, self.color("toolbar.hoverBackground"), 4.0);
                }
                c.icon_in(&icons::CLOSE, close, 12.0, dim);
                self.hits.push((close, Hit::AssistantTabClose(ci)));
            }
            x += w + 4.0;
        }
    }

    /// The shown chat's agent, permission mode and model, each a menu.
    fn draw_chat_header(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        c.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), self.color("sideBarSectionHeader.border"));
        let ci = self.assistant.active;
        let y = r.y + (r.h - 20.0) / 2.0;
        let mut x = r.x + PAD - 6.0;
        let right = r.right() - PAD + 6.0;
        let agent = self.chat_choice(ci).label();
        let menu = self.header_menu(c, x, y, right, &agent, fg, Hit::AssistantAgentMenu);
        self.assistant.menu_at = (menu.x, menu.bottom() + 2.0);
        x = menu.right() + 2.0;
        if let Some((id, name, _)) = self.chat_mode(ci) {
            // Full Access stands out.
            let full = id == "full";
            let color = if full { self.color("editorWarning.foreground") } else { dim };
            if full && x + 18.0 < right {
                c.icon(&icons::WARNING, x + 4.0, y + 3.0, 14.0, color);
                x += 18.0;
            }
            let menu = self.header_menu(c, x, y, right, &name, color, Hit::AssistantModeMenu);
            self.assistant.mode_menu_at = (menu.x, menu.bottom() + 2.0);
            x = menu.right() + 2.0;
        }
        let chat = self.assistant.cur();
        let model = chat.model.clone().or_else(|| chat.cur_model.clone());
        let name = match &model {
            Some(m) => chat.models.iter().find(|x| &x.0 == m).map_or(m.clone(), |x| x.1.clone()),
            None if chat.models.is_empty() => String::new(),
            None => "Default model".into(),
        };
        if !name.is_empty() && x + 40.0 < right {
            let menu = self.header_menu(c, x, y, right, &name, dim, Hit::AssistantModelMenu);
            self.assistant.model_menu_at = (menu.x, menu.bottom() + 2.0);
        }
    }

    /// A label with a chevron that opens a menu; returns where it was drawn.
    fn header_menu(&mut self, c: &mut Canvas, x: f32, y: f32, right: f32, label: &str, color: Color, hit: Hit) -> Rect {
        let st = TextStyle::ui(SMALL, color);
        let w = (c.measure(label, &st) + 26.0).min((right - x).max(30.0));
        let menu = Rect::new(x, y, w, 20.0);
        if self.hovered(hit) {
            c.fill_rounded(menu, self.color("toolbar.hoverBackground"), 5.0);
        }
        c.text_fit(Rect::new(menu.x + 6.0, menu.y, menu.w - 24.0, menu.h), label, &st);
        c.icon(&icons::CHEVRON_DOWN, menu.right() - 17.0, menu.y + 4.0, 12.0, color);
        self.hits.push((menu, hit));
        menu
    }

    /// The chats kept for this folder, newest first.
    fn draw_history(&mut self, c: &mut Canvas, r: Rect, fg: Color, dim: Color) {
        self.assistant.body = r;
        self.hits.push((r, Hit::AssistantBody));
        c.push_clip(r);
        let saved = self.assistant.saved.clone();
        if saved.is_empty() {
            let text = "No chats yet. A chat is kept here, for this folder, once you send a message, until you delete it.";
            let st = TextStyle::ui(UI, dim);
            let mut y = r.y + 16.0;
            for line in super::intel::wrap(c, text, &st, r.w - 2.0 * PAD) {
                c.text(r.x + PAD, y, &line, &st);
                y += 18.0;
            }
            self.assistant.history_h = 0.0;
            c.pop_clip();
            return;
        }
        self.assistant.history_h = 12.0 + HISTORY_ROW * saved.len() as f32;
        let max = (self.assistant.history_h - r.h).max(0.0);
        self.assistant.history_scroll = self.assistant.history_scroll.min(max);
        let now = unix_now() as i64;
        let title_st = TextStyle::ui(UI, fg);
        let small = TextStyle::ui(SMALL, dim);
        for (i, meta) in saved.iter().enumerate() {
            let row = Rect::new(r.x + 6.0, r.y + 6.0 - self.assistant.history_scroll + i as f32 * HISTORY_ROW, r.w - 12.0, HISTORY_ROW - 4.0);
            if row.y > r.bottom() || row.bottom() < r.y {
                continue;
            }
            let hovered = self.hovered(Hit::AssistantHistoryRow(i)) || self.hovered(Hit::AssistantHistoryDelete(i));
            let open = self.chat_index(&meta.id);
            if hovered {
                c.fill_rounded(row, self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let text_w = row.w - 20.0 - if hovered { 26.0 } else { 0.0 };
            c.text_fit(Rect::new(row.x + 10.0, row.y + 3.0, text_w, 18.0), &meta.title, &title_st);
            let agent = agents::from_key(&meta.agent);
            let mut detail = if agent == Choice::None { String::new() } else { format!("{} · ", agent.label()) };
            match super::timeline::age(now - meta.updated as i64).as_str() {
                "now" => detail.push_str("just now"),
                age => detail.push_str(&format!("{age} ago")),
            }
            if let Some(ci) = open {
                detail.push_str(if ci == self.assistant.active { " · shown" } else { " · open" });
            }
            c.text_fit(Rect::new(row.x + 10.0, row.y + 21.0, text_w, 16.0), &detail, &small);
            self.hits.push((row.intersect(&r), Hit::AssistantHistoryRow(i)));
            if hovered {
                let del = Rect::new(row.right() - 28.0, row.y + 9.0, 22.0, 22.0);
                self.icon_button(c, del, &icons::TRASH, Hit::AssistantHistoryDelete(i), dim);
            }
        }
        c.pop_clip();
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
        if self.assistant.cur().entries.is_empty() {
            let name = match self.assistant.cur().agent_name.as_str() {
                "" => match self.chat_choice(self.assistant.active) {
                    Choice::None => "the agent".to_string(),
                    choice => choice.label(),
                },
                name => name.to_string(),
            };
            let hint = format!("Ask {name} about this project, or to change something. It sees the file you're in.");
            let mut y = r.y + 16.0;
            for line in super::intel::wrap(c, &hint, &TextStyle::ui(UI, dim), w) {
                c.text(r.x + PAD, y, &line, &TextStyle::ui(UI, dim));
                y += 18.0;
            }
            self.assistant.cur_mut().content_h = 0.0;
            c.pop_clip();
            return;
        }
        // Lay out every entry, then draw from the bottom up (scrolled by `scroll`).
        let entries = self.assistant.cur().entries.clone();
        let mut blocks = Vec::new();
        let mut total = 8.0;
        for (i, e) in entries.iter().enumerate() {
            let lines = self.entry_lines(c, i, e, w, fg, dim);
            let line_h: f32 = lines.iter().map(|(_, st)| st.line_height.max(16.0)).sum();
            let extra = match e {
                Entry::User(_) => 16.0,
                Entry::Tool(t) => 10.0 + if t.content.iter().any(|c| matches!(c, ToolContent::Diff(_))) { 26.0 } else { 0.0 },
                Entry::Permission(p) => {
                    // Answered: one line instead of the buttons.
                    let buttons = if p.answer.is_some() { 24.0 } else { 32.0 * p.options.len().div_ceil(2).max(1) as f32 };
                    20.0 + buttons + if p.diffs.is_empty() { 0.0 } else { 30.0 }
                }
                Entry::Auth(m) => 20.0 + 32.0 * m.len() as f32,
                Entry::Action(_, actions) => 20.0 + 32.0 * actions.len() as f32,
                Entry::Plan(_) => 12.0,
                _ => 4.0,
            };
            let h = line_h + extra;
            blocks.push((total, h, lines));
            total += h + 10.0;
        }
        let chat = self.assistant.cur_mut();
        chat.content_h = total;
        let max = (total - r.h).max(0.0);
        chat.scroll = chat.scroll.min(max);
        let top = r.y - (max - chat.scroll);
        let turn = (chat.started.map_or(0, |t| t.elapsed().as_millis() / 120) % 8) as u32;
        let agent = chat.agent_name_or_default().to_string();
        self.a11y_list(super::a11y::TRANSCRIPT_LIST, Some(super::a11y::SECONDARY_SIDEBAR), "Conversation", r);
        for (i, (off, h, lines)) in blocks.into_iter().enumerate() {
            let y0 = top + off;
            if y0 > r.bottom() || y0 + h < r.y {
                continue;
            }
            let x = r.x + PAD;
            let read = entry_read(&entries[i], &lines, &agent);
            self.a11y_item(super::a11y::TRANSCRIPT_LIST, i, read, Rect::new(x, y0, w, h).intersect(&r), false);
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
        self.a11y_name(hit, super::a11y::Role::Button, label, false);
        let bg = if self.hovered(hit) { self.color("button.secondaryHoverBackground") } else { self.color("button.secondaryBackground") };
        c.fill_rounded(b, bg, 5.0);
        let st = TextStyle::ui(SMALL, self.color("button.secondaryForeground"));
        let lw = c.measure(label, &st);
        c.text_in(Rect::new(b.x + (b.w - lw) / 2.0, b.y, lw + 2.0, b.h), label, &st);
        self.hits.push((b, hit));
    }

    fn option_button(&mut self, c: &mut Canvas, b: Rect, label: &str, primary: bool, hit: Hit) {
        self.a11y_name(hit, super::a11y::Role::Button, label, false);
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
        // Context chips: the active file (click to leave it out of the next message).
        let file = self.active_doc().and_then(|d| d.buffer.path()).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let x = r.x + PAD;
        let room = r.right() - PAD - x;
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
        let phase = self.assistant.cur().phase;
        let placeholder = match phase {
            Phase::Working => "Working… (Esc stops)",
            Phase::Starting => "Starting the agent…",
            _ => "Ask, or describe a change",
        };
        let caret_on = self.caret_on();
        let sel_bg = self.color("editor.selectionBackground");
        let placeholder_fg = self.color("input.placeholderForeground");
        self.assistant.cur_mut().input.draw(c, Rect::new(field.x + 10.0, field.y, field.w - 14.0, field.h), &st, placeholder, placeholder_fg, focused, caret_on, sel_bg);
        self.hits.push((field, Hit::AssistantInput));
        let button = Rect::new(field.right() + 6.0, field.y + 1.0, 28.0, 28.0);
        if phase == Phase::Working {
            self.icon_button(c, button, &icons::DEBUG_STOP, Hit::AssistantStop, self.color("errorForeground"));
        } else {
            let can_send = !self.assistant.cur().input.text.trim().is_empty();
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
            wb.mcp_tick();
        }
    }

    /// Ticks until `done` holds (or fails after a while).
    fn until(wb: &mut Workbench, r: &mut Option<render::Renderer>, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}: {:#?}", wb.assistant.cur().entries);
            std::thread::sleep(Duration::from_millis(20));
            draw(wb, r);
        }
    }

    fn type_and_send(wb: &mut Workbench, text: &str) {
        wb.run(crate::commands::Command::AssistantFocus);
        wb.assistant.cur_mut().input.set_text(text);
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
        assert_eq!(wb.assistant.cur().entries[0], Entry::User("make it polite".into()));
        until(&mut wb, &mut r, "the permission request", |wb| wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        let (i, p) = wb.assistant.cur().entries.iter().enumerate().find_map(|(i, e)| if let Entry::Permission(p) = e { Some((i, p.clone())) } else { None }).unwrap();
        // The tool call's diff comes with the question.
        assert_eq!(p.diffs.len(), 1);
        assert_eq!(p.diffs[0].new_text, "goodbye world\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "You said: make it polite")));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Thought(_))));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2)));
        // The question was said, and the conversation is a list VoiceOver reads.
        assert!(wb.a11y_note().contains("asks:"), "{}", wb.a11y_note());
        if r.is_some() {
            let list = super::super::a11y::TRANSCRIPT_LIST;
            assert!(wb.a11y_children(Some(list)).iter().any(|&id| wb.a11y_node(id).unwrap().label.starts_with("You: make it polite")));
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
        until(&mut wb, &mut r, "the end of the answer", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.status == "completed")));
        // The reply is read out when the turn ends.
        assert_eq!(wb.a11y_note(), "You said: make it polite");
        assert!(matches!(&wb.assistant.cur().entries[i], Entry::Permission(p) if p.answer.as_deref() == Some("Allow")));
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
        until(&mut wb, &mut r, "the second question", |wb| wb.assistant.cur().entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == 2);
        let j = wb.assistant.cur().entries.iter().rposition(|e| matches!(e, Entry::Permission(_))).unwrap();
        wb.assistant_answer(j, 1);
        until(&mut wb, &mut r, "the end of the second answer", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.status == "failed")));

        // Stopping while it asks answers the question "cancelled".
        type_and_send(&mut wb, "once more");
        until(&mut wb, &mut r, "the third question", |wb| wb.assistant.cur().entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == 3);
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));

        // New Chat: an empty chat next to the first, whose agent keeps running.
        wb.run(crate::commands::Command::AssistantNewChat);
        assert!(wb.assistant.cur().entries.is_empty());
        assert_eq!(wb.assistant.chats.len(), 2);
        assert!(wb.assistant.chats[0].client.is_some() && wb.assistant.cur().client.is_none());

        // An agent that wants a sign-in first: its methods are buttons, picking one goes on.
        // (Without the file, so the agent just answers.)
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "sign in next time");
        until(&mut wb, &mut r, "the answer", |wb| wb.assistant.cur().phase == Phase::Ready && wb.assistant.cur().entries.len() > 2);
        assert!(!wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        wb.run(crate::commands::Command::AssistantNewChat);
        type_and_send(&mut wb, "hello after signing in");
        until(&mut wb, &mut r, "the sign-in", |wb| wb.assistant.cur().phase == Phase::SigningIn);
        let k = wb.assistant.cur().entries.iter().position(|e| matches!(e, Entry::Auth(m) if m[0].1 == "Use a token")).unwrap();
        if r.is_some() {
            let (rect, _) = *wb.hits.iter().find(|(_, h)| *h == Hit::AssistantAuth(k, 0)).unwrap();
            wb.mouse_down(rect.x + 4.0, rect.y + 4.0, false, false, false);
            wb.mouse_up();
        } else {
            wb.assistant_authenticate("token");
        }
        until(&mut wb, &mut r, "the answer after signing in", |wb| wb.assistant.cur().phase == Phase::Ready && matches!(wb.assistant.cur().entries.last(), Some(Entry::Plan(_))));
        assert!(!wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Auth(_))));
        assert_eq!(wb.assistant.chats.len(), 3);

        wb.shutdown();
        assert!(wb.assistant.cur().client.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn workbench(name: &str, settings: &str) -> (Workbench, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbvane-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".orbvane")).unwrap();
        std::fs::write(dir.join(".orbvane/settings.json"), settings).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let _ = std::fs::remove_dir_all(chat_store::dir_for(Some(&dir)));
        (Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {})), dir)
    }

    /// Saves the last frame as `<ORBVANE_ASSISTANT_SNAPSHOT>-<name>.png`, when asked.
    fn snapshot(_wb: &Workbench, r: &Option<render::Renderer>, name: &str) {
        if let (Some(out), Some(r)) = (std::env::var_os("ORBVANE_ASSISTANT_SNAPSHOT"), r) {
            let (w, h, px) = r.pixels();
            render::write_png(std::path::Path::new(&format!("{}-{name}.png", out.to_string_lossy())), w, h, &px).unwrap();
        }
    }

    /// Two chats with their own agents at once, kept on disk: a question waiting in the chat
    /// not shown marks its tab, a closed chat reopens from the History and its agent continues
    /// the conversation (`session/load`), and the next window on the folder reopens the chats.
    #[test]
    fn chats_run_side_by_side_and_are_kept() {
        let agent = std::env::temp_dir().join(format!("orbvane-agents-chats-{}", std::process::id())).join("fake_acp.py");
        let command = serde_json::to_string(&agent.to_string_lossy()).unwrap();
        let (mut wb, dir) = workbench("chats", &format!("{{ \"assistant.agent\": \"custom\", \"assistant.agent.command\": {command} }}"));
        std::fs::write(&agent, include_str!("../../testdata/fake_acp.py")).unwrap();
        std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        wb.open_file(&file);
        let mut r = render::Renderer::offscreen((1200, 760), 1.0).ok();
        let asks = |chat: &Chat| chat.entries.iter().rposition(|e| matches!(e, Entry::Permission(p) if p.answer.is_none()));

        type_and_send(&mut wb, "first");
        until(&mut wb, &mut r, "the first chat's question", |wb| asks(&wb.assistant.chats[0]).is_some());
        wb.run(crate::commands::Command::AssistantNewChat);
        type_and_send(&mut wb, "second");
        until(&mut wb, &mut r, "the second chat's question", |wb| asks(&wb.assistant.chats[1]).is_some());
        assert_eq!(wb.assistant.active, 1);
        assert_eq!(wb.assistant.chats[0].status(), ChatStatus::NeedsYou);
        if r.is_some() {
            assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantTab(0)) && wb.hits.iter().any(|(_, h)| *h == Hit::AssistantTab(1)));
        }
        snapshot(&wb, &r, "tabs");
        let j = asks(wb.assistant.cur()).unwrap();
        wb.assistant_answer(j, 1);
        until(&mut wb, &mut r, "the second answer", |wb| wb.assistant.chats[1].phase == Phase::Ready);
        wb.select_chat(0);
        let i = asks(wb.assistant.cur()).unwrap();
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the first answer", |wb| wb.assistant.chats[0].phase == Phase::Ready);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world\n");
        let titles = |wb: &Workbench| wb.assistant.saved.iter().map(|m| m.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&wb).len(), 2);
        assert!(titles(&wb).contains(&"first".to_string()) && titles(&wb).contains(&"second".to_string()));

        // Closed, it's in the History; reopened, its agent continues where it was.
        let first = wb.assistant.chats[0].id.clone();
        let session = wb.assistant.chats[0].session.clone().unwrap();
        wb.close_chat(0);
        assert_eq!(wb.assistant.chats.len(), 1);
        wb.assistant_toggle_history();
        if let Some(r) = &mut r {
            r.frame(wb.background(), |c| wb.draw(c));
            assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantHistoryRow(1)));
        }
        snapshot(&wb, &r, "history");
        let row = wb.assistant.saved.iter().position(|m| m.id == first).unwrap();
        wb.open_saved_chat(row);
        assert!(!wb.assistant.history);
        assert_eq!(wb.assistant.cur().id, first);
        assert!(matches!(&wb.assistant.cur().entries[0], Entry::User(t) if t == "first"));
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "and then?");
        until(&mut wb, &mut r, "the continued conversation", |wb| wb.assistant.cur().phase == Phase::Ready && matches!(wb.assistant.cur().entries.last(), Some(Entry::Agent(t)) if t == "You said: and then?"));
        assert!(wb.output.lines("Assistant").iter().any(|l| *l == format!("loaded {session}")), "{:?}", wb.output.lines("Assistant"));
        assert_eq!(wb.assistant.cur().session.as_deref(), Some(session.as_str()));
        wb.chat_named(&first, "Renamed".into());

        // The next window on this folder reopens both, showing the same one.
        wb.save_session(false);
        wb.shutdown();
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        let mut labels: Vec<String> = wb.assistant.chats.iter().map(|c| c.label().to_string()).collect();
        labels.sort();
        assert_eq!(labels, ["Renamed", "second"]);
        assert_eq!(wb.assistant.cur().id, first);
        assert!(wb.assistant.chats.iter().all(|c| c.client.is_none() && c.session.is_some()));

        // Deleted: gone from the tabs and the History.
        wb.agent_action(AgentAction::DeleteChat(first.clone()));
        assert_eq!(wb.assistant.chats.len(), 1);
        assert_eq!(titles(&wb), ["second"]);
        assert!(chat_store::load(&chat_store::dir_for(Some(&dir)), &first).is_none());
        wb.shutdown();
        let _ = std::fs::remove_dir_all(chat_store::dir_for(Some(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Action(_, a)) if a[0].1 == AgentAction::SignIn("codex")));
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
        // Drawn only for a snapshot.
        let mut r = std::env::var_os("ORBVANE_ASSISTANT_SNAPSHOT").and_then(|_| render::Renderer::offscreen((1200, 760), 1.0).ok());

        type_and_send(&mut wb, "make it polite");
        // Saved first, since Codex reads the file from disk.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world!\n");
        until(&mut wb, &mut r, "the permission request", |wb| wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        let (i, p) = wb.assistant.cur().entries.iter().enumerate().find_map(|(i, e)| if let Entry::Permission(p) = e { Some((i, p.clone())) } else { None }).unwrap();
        assert_eq!(p.title, "Edit notes.txt");
        assert_eq!(p.diffs.len(), 1);
        assert_eq!(p.diffs[0].new_text, "goodbye world!\n");
        assert_eq!(p.options.iter().map(|o| o.1.as_str()).collect::<Vec<_>>(), ["Allow", "Allow These Files for This Chat", "Reject"]);
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "Using model=m1")), "{:#?}", wb.assistant.cur().entries);
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Thought(t) if t.contains("retired-model") && t.contains("Model One"))));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2 && p[1].status == "in_progress")));
        assert_eq!(wb.assistant.cur().agent_name, "Codex");
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the end of the turn", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world!\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Run `ls -1`" && t.status == "completed")));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "completed")));
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Agent(t)) if t == "Done."), "{:#?}", wb.assistant.cur().entries.last());
        // The editor tools went to Codex's config.
        assert!(wb.output.lines("Assistant").iter().any(|l| l == "mcp servers: ['orbvane']"), "{:?}", wb.output.lines("Assistant"));

        // The chat started in Ask; Auto and another model apply from the next message.
        let log_has = |wb: &Workbench, line: &str| wb.output.lines("Assistant").iter().any(|l| l == line);
        assert!(log_has(&wb, "turn: approval=untrusted sandbox=workspaceWrite model=m1"), "{:?}", wb.output.lines("Assistant"));
        assert_eq!(wb.assistant.cur().cur_mode.as_deref(), Some("ask"));
        assert_eq!(wb.assistant.cur().models.len(), 2);
        wb.agent_action(AgentAction::SetMode("auto".into()));
        wb.agent_action(AgentAction::SetModel("m2".into()));
        until(&mut wb, &mut r, "the new mode and model", |wb| wb.assistant.cur().cur_mode.as_deref() == Some("auto") && wb.assistant.cur().cur_model.as_deref() == Some("m2"));
        draw(&mut wb, &mut r);
        snapshot(&wb, &r, "header");

        // A failed turn: Codex's own message.
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "fail now");
        until(&mut wb, &mut r, "the failure", |wb| wb.assistant.cur().phase == Phase::Ready && matches!(wb.assistant.cur().entries.last(), Some(Entry::Notice(_))));
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Notice(n)) if n == "The agent reported an error: Model is unavailable."));
        assert!(log_has(&wb, "turn: approval=on-request sandbox=workspaceWrite model=m2"), "{:?}", wb.output.lines("Assistant"));

        // Stop interrupts the turn.
        type_and_send(&mut wb, "wait for me");
        until(&mut wb, &mut r, "the turn to start", |wb| wb.assistant.cur().phase == Phase::Working);
        std::thread::sleep(Duration::from_millis(200));
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Claude Code through the bridge, with `testdata/fake_claude.py` standing in for
    /// `claude`: the flags that make it ask us, the stream (thinking, text, the plan), an edit
    /// asked for with its diff and landing on disk, "allow for this chat" remembered for a
    /// command, a rejected edit, an expired sign-in offering Sign In, and Stop.
    /// (Claude Code's own file tools; the editor's are `claude_code_edits_through_the_editor`.)
    #[test]
    fn a_conversation_with_claude_code() {
        let (mut wb, dir) = workbench("claude", r#"{ "assistant.agent": "claude-code", "assistant.permissions": "edits", "assistant.editorFiles": false }"#);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let fake = dir.join("fake_claude.py");
        std::fs::write(&fake, include_str!("../../testdata/fake_claude.py")).unwrap();
        wb.assistant.server_override = Some(format!("exec python3 {} \"$@\"", fake.display()));
        wb.assistant.probes.insert("claude-code", Probe { path: Some("/x/claude".into()), version: "2.1".into(), signed_in: true });
        wb.open_file(&file);
        let mut r = None;
        let question = |wb: &Workbench, n: usize| wb.assistant.cur().entries.iter().filter(|e| matches!(e, Entry::Permission(_))).count() == n;
        let last_question = |wb: &Workbench| wb.assistant.cur().entries.iter().rposition(|e| matches!(e, Entry::Permission(_))).unwrap();

        type_and_send(&mut wb, "make it polite");
        until(&mut wb, &mut r, "the edit question", |wb| question(wb, 1));
        let i = last_question(&wb);
        let Entry::Permission(p) = &wb.assistant.cur().entries[i] else { unreachable!() };
        assert_eq!(p.title, "Edit notes.txt");
        assert_eq!(p.diffs[0].new_text, "goodbye world\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Agent(t) if t == "Using tools")), "{:#?}", wb.assistant.cur().entries);
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Thought(t) if t == "Looking at it.")));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Plan(p) if p.len() == 2 && p[1].status == "in_progress")));
        assert_eq!(wb.assistant.cur().agent_name, "Claude Code");
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the command question", |wb| question(wb, 2));
        let j = last_question(&wb);
        let Entry::Permission(p) = &wb.assistant.cur().entries[j] else { unreachable!() };
        assert_eq!(p.title, "Run `ls -1`");
        assert_eq!(p.options[1].1, "Allow This Command for This Chat");
        wb.assistant_answer(j, 1);
        until(&mut wb, &mut r, "the end of the answer", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye world\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "completed")));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Run `ls -1`" && t.status == "completed")));
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Agent(t)) if t == "Done."), "{:#?}", wb.assistant.cur().entries.last());
        let log = wb.output.lines("Assistant");
        assert!(log.iter().any(|l| l == "flags ok") && log.iter().any(|l| l == "mcp servers: ['orbvane']"), "{log:?}");
        // New chats start in the default mode (here Accept Edits).
        assert!(log.iter().any(|l| l == "mode: acceptEdits"), "{log:?}");
        assert_eq!(wb.assistant.cur().cur_mode.as_deref(), Some("edits"));

        // Again: the edit is asked for (rejected this time), the command isn't.
        std::fs::write(&file, "hello again\n").unwrap();
        type_and_send(&mut wb, "once more");
        until(&mut wb, &mut r, "the second edit question", |wb| question(wb, 3));
        let k = last_question(&wb);
        wb.assistant_answer(k, 2);
        until(&mut wb, &mut r, "the end of the second answer", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(question(&wb, 3), "the command was asked about again");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello again\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "failed")));

        // An expired sign-in: a Sign In button.
        wb.assistant.send_file = false;
        type_and_send(&mut wb, "expired?");
        until(&mut wb, &mut r, "the sign-in prompt", |wb| wb.assistant.cur().phase == Phase::Ready && matches!(wb.assistant.cur().entries.last(), Some(Entry::Action(..))));
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Action(t, a)) if t.contains("sign-in has expired") && a[0].1 == AgentAction::SignIn("claude-code")));

        // Plan mode and another model; approving the plan goes on in Accept Edits.
        wb.agent_action(AgentAction::SetMode("plan".into()));
        wb.agent_action(AgentAction::SetModel("sonnet".into()));
        until(&mut wb, &mut r, "plan mode", |wb| wb.assistant.cur().cur_mode.as_deref() == Some("plan") && wb.assistant.cur().cur_model.as_deref() == Some("sonnet"));
        assert!(wb.output.lines("Assistant").iter().any(|l| l == "model: sonnet"));
        type_and_send(&mut wb, "make a plan");
        until(&mut wb, &mut r, "the plan's question", |wb| question(wb, 4));
        let p = last_question(&wb);
        assert!(matches!(&wb.assistant.cur().entries[p], Entry::Permission(q) if q.title == "Start making the changes in the plan?" && q.options[0].1 == "Yes, and Accept Edits"));
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Agent(t) if t.contains("1. Read it\n2. Change it"))));
        wb.assistant_answer(p, 0);
        until(&mut wb, &mut r, "the approved plan", |wb| wb.assistant.cur().phase == Phase::Ready && matches!(wb.assistant.cur().entries.last(), Some(Entry::Agent(t)) if t.ends_with("Approved.")));
        assert_eq!(wb.assistant.cur().cur_mode.as_deref(), Some("edits"));
        assert_eq!(wb.assistant.cur().mode.as_deref(), Some("edits"));

        // Stop interrupts it.
        type_and_send(&mut wb, "wait for me");
        until(&mut wb, &mut r, "the turn to start", |wb| wb.assistant.cur().phase == Phase::Working);
        std::thread::sleep(Duration::from_millis(200));
        wb.assistant_cancel();
        until(&mut wb, &mut r, "the stop", |wb| wb.assistant.cur().phase == Phase::Ready);
        assert!(matches!(wb.assistant.cur().entries.last(), Some(Entry::Notice(n)) if n == "Stopped."));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the editor's file tools, Claude Code's edits go to the open document: unsaved
    /// changes and all, as an undo step, without asking when the chat accepts edits.
    #[test]
    fn claude_code_edits_through_the_editor() {
        let (mut wb, dir) = workbench("claude-files", r#"{ "assistant.agent": "claude-code", "assistant.permissions": "edits", "assistant.saveBeforeSending": false }"#);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello world\n").unwrap();
        let fake = dir.join("fake_claude.py");
        std::fs::write(&fake, include_str!("../../testdata/fake_claude.py")).unwrap();
        wb.assistant.server_override = Some(format!("exec python3 {} \"$@\"", fake.display()));
        wb.assistant.probes.insert("claude-code", Probe { path: Some("/x/claude".into()), version: "2.1".into(), signed_in: true });
        wb.open_file(&file);
        // An unsaved change the agent should see.
        if let Some((ed, doc)) = wb.active_mut() {
            ed.set_selection(text::Selection::caret(text::Pos::new(0, 5)));
            ed.type_text(doc, " unsaved");
        }
        let mut r = None;
        type_and_send(&mut wb, "make it polite");
        until(&mut wb, &mut r, "the command question", |wb| wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Permission(_))));
        let i = wb.assistant.cur().entries.iter().position(|e| matches!(e, Entry::Permission(_))).unwrap();
        let Entry::Permission(p) = &wb.assistant.cur().entries[i] else { unreachable!() };
        assert_eq!(p.title, "Run `ls -1`", "the edit shouldn't be asked about in Accept Edits");
        wb.assistant_answer(i, 0);
        until(&mut wb, &mut r, "the end of the answer", |wb| wb.assistant.cur().phase == Phase::Ready);
        let doc = wb.active_doc().unwrap();
        assert_eq!((doc.buffer.text().as_str(), doc.buffer.is_dirty()), ("goodbye unsaved world\n", true));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");
        assert!(wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Tool(t) if t.title == "Edit notes.txt" && t.status == "completed")), "{:#?}", wb.assistant.cur().entries);
        assert!(wb.output.lines("Assistant").iter().any(|l| l == "editor files"));
        // Undo (in the editor) takes the agent's edit back.
        wb.focus = Focus::Editor;
        wb.run(crate::commands::Command::Undo);
        assert_eq!(wb.active_doc().unwrap().buffer.text(), "hello unsaved world\n");
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
            let entries = wb.assistant.cur().entries.clone();
            for (i, e) in entries.iter().enumerate() {
                if let Entry::Permission(p) = e {
                    if p.answer.is_none() {
                        eprintln!("asked: {} ({:?}), diffs: {:?}", p.title, p.options.iter().map(|o| &o.1).collect::<Vec<_>>(), p.diffs.iter().map(|d| &d.new_text).collect::<Vec<_>>());
                        wb.assistant_answer(i, 0);
                    }
                }
            }
            for e in &wb.assistant.cur().entries[shown.min(wb.assistant.cur().entries.len())..] {
                eprintln!("entry: {e:?}");
            }
            shown = wb.assistant.cur().entries.len();
            let done = wb.assistant.cur().phase == Phase::Ready || (wb.assistant.cur().client.is_none() && start.elapsed() > Duration::from_secs(5));
            if done || start.elapsed() > Duration::from_secs(240) {
                break;
            }
        }
        eprintln!("final entries: {:#?}", wb.assistant.cur().entries);
        eprintln!("output: {:#?}", wb.output.lines("Assistant"));
        eprintln!("file: {:?}", std::fs::read_to_string(&file));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real agent named by `ORBVANE_REAL_AGENT` continues a chat with a new process: one
    /// message, the agent stopped (as when idle or reopened), then a question only the earlier
    /// conversation answers. Uses the account: `ORBVANE_REAL_AGENT=claude-code cargo test -p app
    /// real_agent_continues -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_agent_continues() {
        let Ok(id) = std::env::var("ORBVANE_REAL_AGENT") else { return };
        let (mut wb, dir) = workbench(&format!("real-resume-{id}"), &format!(r#"{{ "assistant.agent": "{id}" }}"#));
        wb.assistant.send_file = false;
        let mut r = None;
        let answered = |wb: &Workbench, n: usize| {
            let chat = wb.assistant.cur();
            chat.phase == Phase::Ready && chat.entries.iter().filter(|e| matches!(e, Entry::User(_))).count() == n && matches!(chat.entries.last(), Some(Entry::Agent(_)) | Some(Entry::Notice(_)) | Some(Entry::Action(..)))
        };
        let wait = |wb: &mut Workbench, r: &mut Option<render::Renderer>, n: usize| {
            let start = Instant::now();
            while !answered(wb, n) && start.elapsed() < Duration::from_secs(180) {
                std::thread::sleep(Duration::from_millis(50));
                draw(wb, r);
            }
        };
        type_and_send(&mut wb, "Remember the code word PELICAN-42. Reply with just: OK");
        wait(&mut wb, &mut r, 1);
        let session = wb.assistant.cur().session.clone();
        eprintln!("first: {:?} (session {session:?})", wb.assistant.cur().entries.last());
        wb.assistant.cur_mut().stop();
        assert!(wb.assistant.cur().client.is_none() && wb.assistant.cur().session.is_some());
        type_and_send(&mut wb, "What was the code word? Reply with just the word.");
        wait(&mut wb, &mut r, 2);
        let entries = wb.assistant.cur().entries.clone();
        eprintln!("entries: {entries:#?}");
        eprintln!("output: {:#?}", wb.output.lines("Assistant"));
        assert_eq!(wb.assistant.cur().session, session, "the session changed");
        assert!(!entries.iter().any(|e| matches!(e, Entry::Notice(n) if n.contains("new one"))), "it started a new conversation");
        assert!(matches!(entries.last(), Some(Entry::Agent(t)) if t.contains("PELICAN-42")), "{:?}", entries.last());
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
        until(&mut wb, &mut r, "the agent's exit", |wb| wb.assistant.cur().client.is_none() && wb.assistant.cur().entries.iter().any(|e| matches!(e, Entry::Action(..))));
        let Some(Entry::Action(text, actions)) = wb.assistant.cur().entries.last() else { panic!("{:?}", wb.assistant.cur().entries) };
        assert!(text.contains("doesn't seem to speak the Agent Client Protocol"), "{text}");
        assert!(text.contains("usage: no terminal"), "{text}");
        assert_eq!(actions.last().map(|a| &a.1), Some(&AgentAction::Choose));
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// How an entry of the transcript is read: who says it, then what it says as drawn.
fn entry_read(e: &Entry, lines: &[(String, TextStyle)], agent: &str) -> String {
    let text = lines.iter().map(|(l, _)| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
    match e {
        Entry::User(_) => format!("You: {text}"),
        Entry::Agent(_) => format!("{agent}: {text}"),
        Entry::Thought(_) => format!("Thinking: {text}"),
        Entry::Tool(t) => {
            let status = match t.status.as_str() {
                "completed" => "done",
                "failed" => "failed",
                "pending" => "waiting",
                _ => "running",
            };
            format!("{text}, {status}")
        }
        Entry::Plan(p) => {
            let steps: Vec<String> = p
                .iter()
                .map(|s| {
                    let state = match s.status.as_str() {
                        "completed" => "done",
                        "in_progress" => "in progress",
                        _ => "to do",
                    };
                    format!("{}, {state}", s.content)
                })
                .collect();
            format!("Plan: {}", steps.join("; "))
        }
        Entry::Permission(p) => match &p.answer {
            Some(answer) => format!("{agent} asked: {}. Answered: {answer}", p.title),
            None => format!("{agent} asks: {}", p.title),
        },
        Entry::Auth(_) | Entry::Notice(_) | Entry::Action(..) => text,
    }
}

/// Markdown as it's read aloud: no emphasis marks, code fences or heading signs.
fn plain_text(md: &str) -> String {
    md.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .map(|l| l.trim_start_matches(['#', '>', ' ']).replace(['*', '`'], ""))
        .collect::<Vec<_>>()
        .join("\n")
}
