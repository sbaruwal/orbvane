//! Language server management: which server runs for which language, keeping open documents
//! in sync, and turning server responses into editor events.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use language::Lang;
use lsp::{Client, Encoding, Incoming, State};
use serde_json::{json, Value};
use text::{Buffer, Pos};

mod scripts;

/// Language servers we know how to start. Each is used only if its binary is installed.
fn server_for(lang: Lang) -> Option<&'static language::ServerDef> {
    lang.def().server.as_ref()
}

/// A server's semantic token legend: (token types, token modifiers).
fn legend_of(client: &lsp::Client) -> std::sync::Arc<(Vec<String>, Vec<String>)> {
    let legend = &client.capabilities["semanticTokensProvider"]["legend"];
    let names = |key: &str| legend[key].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    std::sync::Arc::new((names("tokenTypes"), names("tokenModifiers")))
}

/// Where tools are usually installed, besides the PATH: apps launched from the Finder don't
/// inherit the shell's PATH.
fn tool_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".cargo/bin"),
        home.join("go/bin"),
        home.join(".volta/bin"),
        home.join(".local/bin"),
        // Homebrew formulas that stay out of its bin folder (rustup's proxies, LLVM's clangd).
        PathBuf::from("/opt/homebrew/opt/rustup/bin"),
        PathBuf::from("/opt/homebrew/opt/llvm/bin"),
        PathBuf::from("/usr/local/opt/rustup/bin"),
        PathBuf::from("/usr/local/opt/llvm/bin"),
    ]
}

/// Marks where the shell's answer starts (profiles may print things first).
const PATH_MARKER: &str = "__ORBVANE_PATH__";

/// The PATH an interactive login shell sets up (what a terminal gets: the profile and rc
/// files), or None if the shell fails or takes over `timeout`.
fn login_shell_path(timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-i", "-l", "-c", &format!("printf '\\n{PATH_MARKER}%s\\n' \"$PATH\"")])
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let path = out.rsplit_once(PATH_MARKER)?.1.lines().next()?.trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Adds the user's shell PATH (when started from the Finder, which doesn't pass it on) and
/// the usual tool directories to our PATH, so we find language servers and the tools we
/// start find theirs too (Delve runs `go`, a Node program runs `npx`). Called first thing in
/// `main`, before any thread.
pub fn extend_path() {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // A terminal sets TERM; without it we were started from the Finder or the Dock.
    if std::env::var_os("TERM").is_none() {
        if let Some(path) = login_shell_path(Duration::from_secs(3)) {
            dirs.extend(std::env::split_paths(&path));
        }
    }
    dirs.extend(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
    dirs.extend(tool_dirs().into_iter().filter(|d| d.is_dir()));
    let mut seen = HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    if let Ok(path) = std::env::join_paths(dirs) {
        // SAFETY: called before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// Finds an executable on PATH or in common install locations.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(tool_dirs());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

pub type ServerKey = (&'static str, PathBuf);

struct OpenDoc {
    key: ServerKey,
    version: i64,
    buffer_version: u64,
}

pub enum Pending {
    Hover { path: PathBuf, pos: Pos },
    Definition,
    Completion { seq: u64 },
    PrepareRename { path: PathBuf, pos: Pos },
    Rename,
    References,
    /// `auto`: the lightbulb's request number (None: asked by Quick Fix).
    CodeActions { auto: Option<u64> },
    ResolveAction,
    /// Formatting edits for `path` at buffer `version`; `save` afterwards (format on save).
    Format { path: PathBuf, version: u64, save: bool },
    /// The symbols of `path` at buffer `version` (the Outline view).
    Symbols { path: PathBuf, version: u64 },
    /// Signature help request number `seq`.
    SignatureHelp { seq: u64 },
    /// `workspace/symbol` request number `seq`.
    WorkspaceSymbols { seq: u64 },
    /// Inlay hints for `path` at buffer `version`.
    InlayHints { path: PathBuf, version: u64 },
    /// Semantic tokens for `path` at buffer `version`.
    SemanticTokens { path: PathBuf, version: u64 },
    /// Folding ranges for `path` at buffer `version`.
    FoldingRanges { path: PathBuf, version: u64 },
    DocumentColors { path: PathBuf, version: u64 },
    /// Code lenses for `path` at buffer `version`.
    CodeLenses { path: PathBuf, version: u64 },
    /// Code lens `index` of `path` at buffer `version`, filled in by `codeLens/resolve`.
    ResolveLens { path: PathBuf, version: u64, index: usize },
    /// `textDocument/prepareCallHierarchy`.
    PrepareCalls,
    PrepareTypes,
    /// `typeHierarchy/supertypes` or `subtypes` of type tree node `node`.
    Types { node: usize, supertypes: bool, seq: u64 },
    /// The incoming or outgoing calls of call tree node `node` (request number `seq`).
    Calls { node: usize, incoming: bool, seq: u64 },
    /// `textDocument/linkedEditingRange` for request `seq`.
    LinkedEditing { seq: u64 },
    /// `html/autoInsert` after typing at `pos` of `path` (buffer `version`).
    AutoInsert { path: PathBuf, version: u64, pos: Pos },
    /// A request made for a feature outside the editor core (a test provider); the answer
    /// goes back to whoever holds its id.
    Ext,
}

pub enum Event {
    Hover { path: PathBuf, pos: Pos, markdown: String },
    Definition { locations: Vec<lsp::Location>, encoding: Encoding },
    Completion { seq: u64, items: Vec<lsp::CompletionItem>, incomplete: bool, encoding: Encoding },
    /// The symbol at `pos` can be renamed (`range`: its extent and maybe a placeholder), or
    /// can't (`range` None, with the server's reason if it gave one).
    PrepareRename { path: PathBuf, pos: Pos, range: Option<(lsp::Range, Option<String>)>, error: Option<String>, encoding: Encoding },
    /// Edits to apply (a rename result, or the server's `workspace/applyEdit`). `reply` is
    /// set when the server waits for an answer.
    Edit { edit: lsp::WorkspaceEdit, encoding: Encoding, reply: Option<(ServerKey, Value)> },
    References { locations: Vec<lsp::Location>, encoding: Encoding },
    /// Code actions for the range asked about, from the server `key`; `auto` for the lightbulb.
    CodeActions { actions: Vec<lsp::CodeAction>, encoding: Encoding, key: ServerKey, auto: Option<u64> },
    Formatted { path: PathBuf, version: u64, edits: Vec<lsp::TextEdit>, encoding: Encoding, save: bool },
    /// A code action filled in by `codeAction/resolve`.
    ResolvedAction { action: lsp::CodeAction, encoding: Encoding, key: ServerKey },
    /// The symbols of `path` at buffer `version`; None if the server couldn't answer yet.
    Symbols { path: PathBuf, version: u64, symbols: Option<Vec<lsp::DocumentSymbol>>, encoding: Encoding },
    /// The answer to signature help request `seq` (None: nothing to show).
    SignatureHelp { seq: u64, help: Option<lsp::SignatureHelp> },
    /// Semantic tokens for `path` at buffer `version`: the raw `data` (5 numbers per token) and
    /// the server's legend (type and modifier names). None: the server couldn't answer.
    SemanticTokens { path: PathBuf, version: u64, data: Option<Vec<u32>>, legend: std::sync::Arc<(Vec<String>, Vec<String>)>, encoding: Encoding },
    /// Folding ranges for `path` at buffer `version`: (start line, end line). None: the server
    /// couldn't answer.
    FoldingRanges { path: PathBuf, version: u64, ranges: Option<Vec<(usize, usize)>> },
    /// `textDocument/documentColor`: each color's range and RGBA (0..1). None: the request failed.
    DocumentColors { path: PathBuf, version: u64, colors: Option<Vec<(lsp::Range, [f32; 4])>>, encoding: Encoding },
    /// Inlay hints for `path` at buffer `version` (None: the server couldn't answer).
    InlayHints { path: PathBuf, version: u64, hints: Option<Vec<lsp::InlayHint>>, encoding: Encoding },
    /// One server's matches for `workspace/symbol` request number `seq`.
    WorkspaceSymbols { seq: u64, symbols: Vec<lsp::WorkspaceSymbol>, encoding: Encoding },
    /// Code lenses for `path` at buffer `version`, as the server sent them (None: it couldn't
    /// answer yet).
    CodeLenses { path: PathBuf, version: u64, lenses: Option<Vec<Value>>, encoding: Encoding },
    /// Code lens `index` of `path`, resolved.
    LensResolved { path: PathBuf, version: u64, index: usize, lens: Value },
    /// The call hierarchy items at a position (usually one).
    CallRoots { items: Vec<Value>, encoding: Encoding },
    /// `textDocument/prepareTypeHierarchy`: the type at the cursor.
    TypeRoots { items: Vec<Value>, encoding: Encoding },
    /// A type's supertypes or subtypes (TypeHierarchyItems).
    Types { node: usize, supertypes: bool, seq: u64, items: Vec<Value>, encoding: Encoding },
    /// Node `node`'s calls: each `{ from | to, fromRanges }`.
    Calls { node: usize, incoming: bool, seq: u64, calls: Vec<Value>, encoding: Encoding },
    /// A snippet to insert at `pos` of `path` (buffer `version`): the end tag after `>`, or
    /// quotes after `=` (`html/autoInsert`).
    AutoInsert { path: PathBuf, version: u64, pos: Pos, snippet: String },
    /// The answer to request `id` made with `Servers::ext_request`.
    ExtResponse { key: ServerKey, id: i64, result: Result<Value, String> },
    /// Linked editing ranges for request `seq` (None: there are none), with the server's
    /// word pattern for what they may contain.
    LinkedEditing { seq: u64, ranges: Option<Vec<lsp::Range>>, word_pattern: Option<String>, encoding: Encoding },
    /// A notification the editor core doesn't handle (a server's own extensions, like
    /// rust-analyzer's `experimental/changeTestState`).
    ExtNotification { key: ServerKey, method: String, params: Value },
    /// A request failed; the message is for the user.
    Failed { message: String },
}

/// `languageServers.stopWhenIdle`: which servers stop when idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleStop {
    Off,
    /// Servers without `heavy` set.
    Light,
    All,
}

impl IdleStop {
    pub fn parse(s: &str) -> Self {
        match s {
            "off" => IdleStop::Off,
            "all" => IdleStop::All,
            _ => IdleStop::Light,
        }
    }
}

/// Whether a line a server printed while starting says it isn't installed (rustup's
/// placeholder for a component that hasn't been added).
fn says_not_installed(line: &str) -> bool {
    line.contains("is not installed") || line.contains("Unknown binary")
}

/// A language server that isn't installed.
#[derive(Clone, Debug, PartialEq)]
pub struct MissingServer {
    pub language: &'static str,
    pub command: &'static str,
    /// The command that installs it, if the language knows one.
    pub install: Option<&'static str>,
}

pub struct Servers {
    waker: lsp::Waker,
    clients: HashMap<ServerKey, Client>,
    /// Servers that failed to start or crashed; not retried this session.
    failed: HashSet<ServerKey>,
    /// Servers that aren't installed, for the editor to tell the user (`take_missing`).
    missing: Vec<MissingServer>,
    /// Servers stopped with Stop Language Servers; they don't start again until restarted.
    held: HashSet<ServerKey>,
    /// When each running server last had one of its files on screen (or was asked something).
    last_used: HashMap<ServerKey, Instant>,
    /// Servers with a file on screen (never idle).
    shown: HashSet<ServerKey>,
    /// Running servers that take long to load a project (`ServerDef::heavy`).
    heavy: HashSet<ServerKey>,
    /// Which server published each file's diagnostics.
    diagnostics_from: HashMap<PathBuf, ServerKey>,
    /// Servers that said, while starting, that they aren't installed.
    not_installed: HashSet<ServerKey>,
    docs: HashMap<PathBuf, OpenDoc>,
    pending: HashMap<(ServerKey, i64), Pending>,
    /// Diagnostics per file, with the encoding their columns use.
    pub diagnostics: BTreeMap<PathBuf, (Encoding, Vec<lsp::Diagnostic>)>,
    /// Files whose diagnostics were just published (the editor re-anchors them to the text).
    pub published: Vec<PathBuf>,
    /// In-progress work reported by servers, by token.
    progress: BTreeMap<String, (String, String)>,
    /// A server asked for its inlay hints to be fetched again.
    pub inlay_refresh: bool,
    /// A server asked for its code lenses to be fetched again.
    pub lens_refresh: bool,
    /// Counts finished server work (indexing, checking): answers may have changed since.
    pub work_done: u64,
    pub output: Vec<String>,
    /// Initialization options the editor adds for its built-in servers (the JSON server's
    /// schemas), by command.
    builtin_options: HashMap<&'static str, Value>,
    /// HTML files' JavaScript, as documents of their own (`scripts.rs`), and back.
    embedded: HashMap<PathBuf, scripts::Embedded>,
    host_of: HashMap<PathBuf, PathBuf>,
}

impl Servers {
    pub fn new(waker: lsp::Waker) -> Self {
        Self {
            waker,
            clients: HashMap::new(),
            failed: HashSet::new(),
            missing: Vec::new(),
            held: HashSet::new(),
            last_used: HashMap::new(),
            shown: HashSet::new(),
            heavy: HashSet::new(),
            diagnostics_from: HashMap::new(),
            not_installed: HashSet::new(),
            docs: HashMap::new(),
            pending: HashMap::new(),
            diagnostics: BTreeMap::new(),
            published: Vec::new(),
            progress: BTreeMap::new(),
            work_done: 0,
            inlay_refresh: false,
            lens_refresh: false,
            output: Vec::new(),
            builtin_options: HashMap::new(),
            embedded: HashMap::new(),
            host_of: HashMap::new(),
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        self.output.push(line.into());
        if self.output.len() > 5000 {
            self.output.drain(..1000);
        }
    }

    /// Starts the server for `lang` in `root` if it isn't running yet (a feature needs it
    /// before any of its files is open). None if there's no such server.
    pub fn start(&mut self, lang: Lang, root: &Path) -> Option<ServerKey> {
        self.ensure_client(lang, root)
    }

    /// The server that would handle `lang` in `root`, whether or not it runs.
    pub fn key_of(lang: Lang, root: &Path) -> Option<ServerKey> {
        server_for(lang).map(|s| (s.command, root.to_path_buf()))
    }

    /// Whether server `key` has started (None: it isn't running and won't be: not
    /// installed, or it failed).
    pub fn ready(&self, key: &ServerKey) -> Option<bool> {
        match self.clients.get(key) {
            Some(c) => Some(c.state == State::Running),
            None if self.failed.contains(key) => None,
            None => Some(false),
        }
    }

    /// Whether server `key` is running or starting.
    pub fn is_started(&self, key: &ServerKey) -> bool {
        self.clients.contains_key(key)
    }

    /// Sends request `method` to server `key` for a feature outside the editor core; its
    /// answer comes back as `Event::ExtResponse` with the returned id.
    pub fn ext_request(&mut self, key: &ServerKey, method: &str, params: Value) -> Option<i64> {
        let client = self.clients.get_mut(key)?;
        let id = client.request(method, params);
        if let Some(t) = self.last_used.get_mut(key) {
            *t = Instant::now();
        }
        self.pending.insert((key.clone(), id), Pending::Ext);
        Some(id)
    }

    /// Sends `method` about the document at `path` (opened with `sync` first), with `pos` as
    /// its position if given; the answer comes back as `Event::ExtResponse`.
    pub fn doc_request(&mut self, path: &Path, buffer: &Buffer, method: &str, mut params: Value, pos: Option<Pos>) -> Option<(ServerKey, i64)> {
        self.send_changes(path, buffer);
        self.touch(path);
        let key = self.docs.get(path).map(|d| d.key.clone())?;
        let client = self.clients.get_mut(&key)?;
        params["textDocument"] = json!({ "uri": lsp::path_to_uri(path) });
        if let Some(pos) = pos {
            let character = client.encoding.to_lsp(&buffer.line(pos.line), pos.col);
            params["position"] = json!({ "line": pos.line, "character": character });
        }
        let id = client.request(method, params);
        self.pending.insert((key.clone(), id), Pending::Ext);
        Some((key, id))
    }

    pub fn ext_notify(&mut self, key: &ServerKey, method: &str, params: Value) {
        if let Some(client) = self.clients.get_mut(key) {
            client.notify(method, params);
        }
    }

    /// Initialization options for built-in server `command` ("builtin:json"), merged over the
    /// ones in `languages.json`.
    /// Returns whether they changed.
    pub fn set_builtin_options(&mut self, command: &'static str, options: Value) -> bool {
        self.builtin_options.insert(command, options.clone()).is_none_or(|old| old != options)
    }

    /// Starts a server that runs inside the editor (`"command": "builtin:<name>"`).
    fn start_builtin(&mut self, server: &language::ServerDef, root: &Path) -> std::io::Result<Client> {
        let serve = match server.command {
            "builtin:json" => json::serve,
            "builtin:css" => css::serve,
            "builtin:html" => html::serve,
            other => return Err(std::io::Error::other(format!("there is no built-in server {other}"))),
        };
        let mut options = server.initialization_options.clone();
        if let Some(extra) = self.builtin_options.get(server.command).and_then(Value::as_object) {
            if !options.is_object() {
                options = json!({});
            }
            for (k, v) in extra {
                options[k] = v.clone();
            }
        }
        Client::in_process(server.command, root, options, server.settings.clone(), self.waker.clone(), serve)
    }

    /// The servers found missing since the last call.
    pub fn take_missing(&mut self) -> Vec<MissingServer> {
        std::mem::take(&mut self.missing)
    }

    fn ensure_client(&mut self, lang: Lang, root: &Path) -> Option<ServerKey> {
        let server = server_for(lang)?;
        let binary = server.command;
        let key = (binary, root.to_path_buf());
        if self.clients.contains_key(&key) {
            return Some(key);
        }
        if self.failed.contains(&key) || self.held.contains(&key) {
            return None;
        }
        self.last_used.insert(key.clone(), Instant::now());
        if server.heavy {
            self.heavy.insert(key.clone());
        }
        if binary.starts_with("builtin:") {
            return match self.start_builtin(server, root) {
                Ok(client) => {
                    self.clients.insert(key.clone(), client);
                    Some(key)
                }
                Err(e) => {
                    self.log(format!("[{binary}] failed to start: {e}"));
                    self.failed.insert(key);
                    None
                }
            };
        }
        let Some(path) = find_binary(binary) else {
            self.log(format!("[{binary}] not found on PATH; {} language features are off", lang.name()));
            self.missing.push(MissingServer { language: lang.name(), command: binary, install: server.install });
            self.failed.insert(key);
            return None;
        };
        match Client::spawn(binary, &path, &server.args, root, server.initialization_options.clone(), server.settings.clone(), self.waker.clone()) {
            Ok(client) => {
                self.log(format!("[{binary}] started in {}", root.display()));
                self.clients.insert(key.clone(), client);
                Some(key)
            }
            Err(e) => {
                self.log(format!("[{binary}] failed to start: {e}"));
                self.failed.insert(key);
                None
            }
        }
    }

    /// Forgets that `command` wasn't installed (it just was), so it starts when next needed.
    pub fn forget_missing(&mut self, command: &str) {
        self.failed.retain(|k| k.0 != command);
        self.not_installed.retain(|k| k.0 != command);
    }

    /// Whether `path` has been opened with a server (it may have stopped since).
    pub fn is_open(&self, path: &Path) -> bool {
        self.docs.contains_key(path)
    }

    /// `path`'s server was just used.
    fn touch(&mut self, path: &Path) {
        if let Some(doc) = self.docs.get(path) {
            if let Some(t) = self.last_used.get_mut(&doc.key) {
                *t = Instant::now();
            }
        }
    }

    /// The files on screen: their servers aren't idle, and the ones whose files just went
    /// out of sight start counting from now.
    pub fn set_shown<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) {
        let paths: Vec<&Path> = paths.into_iter().flat_map(|p| [Some(p), self.script_doc(p)]).flatten().collect();
        let shown: HashSet<ServerKey> = paths.into_iter().filter_map(|p| self.docs.get(p)).map(|d| d.key.clone()).collect();
        let now = Instant::now();
        for key in self.shown.difference(&shown) {
            if let Some(t) = self.last_used.get_mut(key) {
                *t = now;
            }
        }
        self.shown = shown;
    }

    /// The servers running or starting.
    pub fn running(&self) -> Vec<ServerKey> {
        let mut keys: Vec<ServerKey> = self.clients.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Whether server `key` was stopped with Stop Language Servers.
    pub fn is_held(&self, key: &ServerKey) -> bool {
        self.held.contains(key)
    }

    /// Stops server `key`. Its documents count as closed (they open again with the next
    /// server); `clear` drops its diagnostics too, else they stay until a server publishes
    /// new ones.
    fn stop(&mut self, key: &ServerKey, clear: bool) {
        let Some(client) = self.clients.remove(key) else { return };
        client.shutdown_in_background();
        if clear {
            self.drop_script_diagnostics(key);
        }
        self.docs.retain(|_, d| d.key != *key);
        self.pending.retain(|(k, _), _| k != key);
        let prefix = format!("{}: ", key.0);
        self.progress.retain(|_, (title, _)| !title.starts_with(&prefix));
        self.last_used.remove(key);
        self.heavy.remove(key);
        self.shown.remove(key);
        if clear {
            let paths: Vec<PathBuf> = self.diagnostics_from.iter().filter(|(_, k)| *k == key).map(|(p, _)| p.clone()).collect();
            for path in paths {
                self.diagnostics_from.remove(&path);
                if self.diagnostics.remove(&path).is_some() {
                    self.published.push(path);
                }
            }
        }
    }

    /// Restarts the servers `keys` (also ones that crashed or were stopped): they stop now and
    /// start again when one of their files is shown or a feature needs them.
    pub fn restart(&mut self, keys: &[ServerKey]) {
        for key in keys {
            self.log(format!("[{}] restarting", key.0));
            self.stop(key, true);
            self.failed.remove(key);
            self.held.remove(key);
        }
    }

    /// Stops every server until it's restarted (Restart Language Server), to free memory.
    /// Returns how many there were.
    pub fn stop_all(&mut self) -> usize {
        let keys = self.running();
        for key in &keys {
            self.log(format!("[{}] stopped", key.0));
            self.stop(key, true);
            self.held.insert(key.clone());
        }
        keys.len()
    }

    /// The servers that may stop when idle and aren't busy, with when they became idle.
    fn idle_candidates(&self, mode: IdleStop) -> impl Iterator<Item = (&ServerKey, Instant)> {
        self.clients.keys().filter_map(move |key| {
            let stoppable = !self.shown.contains(key) && match mode {
                IdleStop::Off => false,
                IdleStop::Light => !self.heavy.contains(key),
                IdleStop::All => true,
            };
            let prefix = format!("{}: ", key.0);
            let busy = self.pending.keys().any(|(k, _)| k == key) || self.progress.values().any(|(t, _)| t.starts_with(&prefix));
            (stoppable && !busy).then(|| (key, self.last_used.get(key).copied().unwrap_or_else(Instant::now)))
        })
    }

    /// Stops the servers idle for `after`. Returns the ones stopped.
    pub fn stop_idle(&mut self, mode: IdleStop, after: Duration) -> Vec<ServerKey> {
        let now = Instant::now();
        let idle: Vec<ServerKey> = self.idle_candidates(mode).filter(|(_, t)| now.duration_since(*t) >= after).map(|(k, _)| k.clone()).collect();
        for key in &idle {
            self.log(format!("[{}] stopped after {} idle minutes; it starts again when one of its files is shown", key.0, after.as_secs() / 60));
            self.stop(key, false);
        }
        idle
    }

    /// When the next server becomes idle for `after`.
    pub fn idle_deadline(&self, mode: IdleStop, after: Duration) -> Option<Instant> {
        self.idle_candidates(mode).map(|(_, t)| t + after).min()
    }

    pub fn has_server(&self, path: &Path) -> bool {
        self.docs.get(path).is_some_and(|d| self.clients.get(&d.key).is_some_and(|c| c.state != State::Exited))
    }

    /// Whether the server for `path` has finished starting (its capabilities are known).
    pub fn is_running(&self, path: &Path) -> bool {
        self.docs.get(path).is_some_and(|d| self.clients.get(&d.key).is_some_and(|c| c.state == State::Running))
    }

    /// The characters that open signature help, and the ones that update it while it's open.
    /// (An HTML file's include its scripts' server's.)
    pub fn signature_triggers(&self, path: &Path) -> (Vec<String>, Vec<String>) {
        let clients: Vec<&Client> = [Some(path), self.script_doc(path)].into_iter().flatten().filter_map(|p| self.docs.get(p)).filter_map(|d| self.clients.get(&d.key)).collect();
        let list = |key: &str| -> Vec<String> {
            let all = clients.iter().filter_map(|c| c.capabilities["signatureHelpProvider"][key].as_array()).flatten();
            let mut out: Vec<String> = all.filter_map(|v| v.as_str().map(str::to_string)).collect();
            out.dedup();
            out
        };
        (list("triggerCharacters"), list("retriggerCharacters"))
    }

    /// Asks for signature help at `pos`. `context`: the SignatureHelpContext.
    pub fn signature_help(&mut self, path: &Path, buffer: &Buffer, pos: Pos, context: Value, seq: u64) {
        self.request(path, "textDocument/signatureHelp", json!({ "context": context }), buffer, pos, Pending::SignatureHelp { seq });
    }

    pub fn completion_triggers(&self, path: &Path) -> &[String] {
        self.docs.get(path).and_then(|d| self.clients.get(&d.key)).map_or(&[], |c| c.completion_triggers.as_slice())
    }

    /// Opens the document with its server, or sends its new contents if it changed.
    pub fn sync(&mut self, path: &Path, lang: Lang, buffer: &Buffer, root: &Path) {
        if lang.id() == "html" {
            self.sync_scripts(path, buffer, root);
        }
        if self.docs.contains_key(path) {
            return self.send_changes(path, buffer);
        }
        let Some(key) = self.ensure_client(lang, root) else { return };
        let language_id = lang.id();
        if let Some(client) = self.clients.get_mut(&key) {
            client.notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": lsp::path_to_uri(path), "languageId": language_id, "version": 1, "text": buffer.text()
                }}),
            );
        }
        self.docs.insert(path.to_path_buf(), OpenDoc { key, version: 1, buffer_version: buffer.version() });
    }

    pub fn close(&mut self, path: &Path) {
        self.close_scripts(path);
        if let Some(doc) = self.docs.remove(path) {
            if let Some(client) = self.clients.get_mut(&doc.key) {
                client.notify("textDocument/didClose", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
            }
        }
    }

    pub fn saved(&mut self, path: &Path) {
        if let Some(client) = self.docs.get(path).and_then(|d| self.clients.get_mut(&d.key)) {
            client.notify("textDocument/didSave", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        }
    }

    /// Whether the server for `path` offers `capability` (a key of its ServerCapabilities).
    pub fn supports(&self, path: &Path, capability: &str) -> bool {
        let in_scripts = matches!(capability, "hoverProvider" | "completionProvider" | "signatureHelpProvider" | "definitionProvider" | "referencesProvider");
        self.supports_here(path, capability) || in_scripts && self.script_doc(path).is_some_and(|s| self.supports_here(s, capability))
    }

    fn supports_here(&self, path: &Path, capability: &str) -> bool {
        self.docs.get(path).and_then(|d| self.clients.get(&d.key)).is_some_and(|c| {
            let v = &c.capabilities[capability];
            !v.is_null() && *v != Value::Bool(false)
        })
    }

    pub fn prepare_rename(&mut self, path: &Path, buffer: &Buffer, pos: Pos) {
        let pending = Pending::PrepareRename { path: path.to_path_buf(), pos };
        self.request(path, "textDocument/prepareRename", json!({}), buffer, pos, pending);
    }

    /// Code actions for `range` (quick fixes for `diagnostics` there, refactorings). `auto`:
    /// asked because the cursor moved (the lightbulb's request number), not by the user.
    pub fn code_actions(&mut self, path: &Path, buffer: &Buffer, range: (Pos, Pos), diagnostics: Vec<Value>, auto: Option<u64>) {
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return };
        let Some(client) = self.clients.get_mut(&key) else { return };
        let enc = client.encoding;
        let at = |p: Pos| json!({ "line": p.line, "character": enc.to_lsp(&buffer.line(p.line), p.col) });
        let params = json!({
            "textDocument": { "uri": lsp::path_to_uri(path) },
            "range": { "start": at(range.0), "end": at(range.1) },
            "context": { "diagnostics": diagnostics, "triggerKind": if auto.is_some() { 2 } else { 1 } },
        });
        let id = client.request("textDocument/codeAction", params);
        self.pending.insert((key, id), Pending::CodeActions { auto });
    }

    /// Asks for the symbols of `path` (the buffer at its current version). Returns false when
    /// no server for it offers them.
    pub fn document_symbols(&mut self, path: &Path, buffer: &Buffer) -> bool {
        if !self.supports(path, "documentSymbolProvider") {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let id = client.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        self.pending.insert((key, id), Pending::Symbols { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Asks for the document's folding ranges. Returns false when its server doesn't offer them.
    pub fn folding_ranges(&mut self, path: &Path, buffer: &Buffer) -> bool {
        if !self.supports(path, "foldingRangeProvider") {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let id = client.request("textDocument/foldingRange", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        self.pending.insert((key, id), Pending::FoldingRanges { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Asks for the document's colors (the swatches). Returns false when its server doesn't
    /// offer them.
    pub fn document_colors(&mut self, path: &Path, buffer: &Buffer) -> bool {
        if !self.supports(path, "colorProvider") {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let id = client.request("textDocument/documentColor", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        self.pending.insert((key, id), Pending::DocumentColors { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Asks for the whole document's semantic tokens. Returns false when its server doesn't
    /// offer them.
    pub fn semantic_tokens(&mut self, path: &Path, buffer: &Buffer) -> bool {
        let full = self.docs.get(path).and_then(|d| self.clients.get(&d.key)).is_some_and(|c| {
            let f = &c.capabilities["semanticTokensProvider"]["full"];
            !f.is_null() && *f != Value::Bool(false)
        });
        if !full {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let id = client.request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        self.pending.insert((key, id), Pending::SemanticTokens { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Asks for the inlay hints of lines `lines` of `path`. Returns false when its server
    /// doesn't offer them.
    pub fn inlay_hints(&mut self, path: &Path, buffer: &Buffer, lines: std::ops::Range<usize>) -> bool {
        if !self.supports(path, "inlayHintProvider") {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        // Ends at the end of the last line asked about (a line past the text is invalid).
        let last = lines.end.min(buffer.len_lines()).max(1) - 1;
        let end_char = client.encoding.to_lsp(&buffer.line(last), buffer.line_len(last));
        let range = json!({ "start": { "line": lines.start, "character": 0 }, "end": { "line": last, "character": end_char } });
        let id = client.request("textDocument/inlayHint", json!({ "textDocument": { "uri": lsp::path_to_uri(path) }, "range": range }));
        self.pending.insert((key, id), Pending::InlayHints { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Asks for the code lenses of `path`; false if its server has none.
    pub fn code_lenses(&mut self, path: &Path, buffer: &Buffer) -> bool {
        if !self.supports(path, "codeLensProvider") {
            return false;
        }
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let id = client.request("textDocument/codeLens", json!({ "textDocument": { "uri": lsp::path_to_uri(path) } }));
        self.pending.insert((key, id), Pending::CodeLenses { path: path.to_path_buf(), version: buffer.version() });
        true
    }

    /// Fills in a lens that came without its command (`codeLens/resolve`).
    pub fn resolve_code_lens(&mut self, path: &Path, version: u64, index: usize, lens: Value) -> bool {
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        if client.capabilities["codeLensProvider"]["resolveProvider"].as_bool() != Some(true) {
            return false;
        }
        let id = client.request("codeLens/resolve", lens);
        self.pending.insert((key, id), Pending::ResolveLens { path: path.to_path_buf(), version, index });
        true
    }

    /// The server handling `path`.
    pub fn key_for(&self, path: &Path) -> Option<ServerKey> {
        self.docs.get(path).map(|d| d.key.clone())
    }

    /// Searches every running server offering workspace symbols for `query`. Returns how
    /// many were asked (each answers with an `Event::WorkspaceSymbols`).
    pub fn workspace_symbols(&mut self, query: &str, seq: u64) -> usize {
        let mut asked = 0;
        for (key, client) in &mut self.clients {
            let v = &client.capabilities["workspaceSymbolProvider"];
            if client.state != State::Running || v.is_null() || *v == Value::Bool(false) {
                continue;
            }
            let id = client.request("workspace/symbol", json!({ "query": query }));
            self.pending.insert((key.clone(), id), Pending::WorkspaceSymbols { seq });
            asked += 1;
        }
        asked
    }

    /// Formats the document, or `range` of it. `options`: FormattingOptions.
    pub fn format(&mut self, path: &Path, buffer: &Buffer, range: Option<(Pos, Pos)>, options: Value, save: bool) -> bool {
        self.send_changes(path, buffer);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get_mut(&key) else { return false };
        let enc = client.encoding;
        let at = |p: Pos| json!({ "line": p.line, "character": enc.to_lsp(&buffer.line(p.line), p.col) });
        let mut params = json!({ "textDocument": { "uri": lsp::path_to_uri(path) }, "options": options });
        let method = match range {
            Some((a, z)) => {
                params["range"] = json!({ "start": at(a), "end": at(z) });
                "textDocument/rangeFormatting"
            }
            None => "textDocument/formatting",
        };
        let id = client.request(method, params);
        self.pending.insert((key, id), Pending::Format { path: path.to_path_buf(), version: buffer.version(), save });
        true
    }

    /// Format on type: asks for edits after `ch` was typed at `pos` (the position after it), if
    /// it's one of the server's trigger characters.
    pub fn format_on_type(&mut self, path: &Path, buffer: &Buffer, pos: Pos, ch: char, options: Value) -> bool {
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return false };
        let Some(client) = self.clients.get(&key) else { return false };
        let caps = &client.capabilities["documentOnTypeFormattingProvider"];
        let s = ch.to_string();
        let triggers = caps["firstTriggerCharacter"].as_str() == Some(s.as_str())
            || caps["moreTriggerCharacter"].as_array().is_some_and(|a| a.iter().any(|c| c.as_str() == Some(s.as_str())));
        if !triggers {
            return false;
        }
        self.send_changes(path, buffer);
        let client = self.clients.get_mut(&key).unwrap();
        let enc = client.encoding;
        let position = json!({ "line": pos.line, "character": enc.to_lsp(&buffer.line(pos.line), pos.col) });
        let params = json!({ "textDocument": { "uri": lsp::path_to_uri(path) }, "position": position, "ch": s, "options": options });
        let id = client.request("textDocument/onTypeFormatting", params);
        self.pending.insert((key, id), Pending::Format { path: path.to_path_buf(), version: buffer.version(), save: false });
        true
    }

    /// Fills in a code action that came without its edit.
    pub fn resolve_code_action(&mut self, key: &ServerKey, action: &lsp::CodeAction) {
        let Some(client) = self.clients.get_mut(key) else { return };
        let id = client.request("codeAction/resolve", action.raw.clone());
        self.pending.insert((key.clone(), id), Pending::ResolveAction);
    }

    /// Runs a code action's command on the server (it may answer with `workspace/applyEdit`).
    /// Returns false if the server doesn't implement it (a command meant for its editor
    /// extension).
    pub fn execute_command(&mut self, key: &ServerKey, command: &Value) -> bool {
        let Some(client) = self.clients.get_mut(key) else { return false };
        let name = command["command"].as_str().unwrap_or_default();
        let supported = client.capabilities["executeCommandProvider"]["commands"]
            .as_array()
            .is_some_and(|cmds| cmds.iter().any(|c| c.as_str() == Some(name)));
        if !supported {
            return false;
        }
        let args = command.get("arguments").cloned().unwrap_or(json!([]));
        client.request("workspace/executeCommand", json!({ "command": name, "arguments": args }));
        true
    }

    pub fn references(&mut self, path: &Path, buffer: &Buffer, pos: Pos) {
        let params = json!({ "context": { "includeDeclaration": true } });
        self.request(path, "textDocument/references", params, buffer, pos, Pending::References);
    }

    pub fn rename(&mut self, path: &Path, buffer: &Buffer, pos: Pos, new_name: &str) {
        self.request(path, "textDocument/rename", json!({ "newName": new_name }), buffer, pos, Pending::Rename);
    }

    /// Answers a server's request (`workspace/applyEdit`).
    pub fn respond(&mut self, key: &ServerKey, id: Value, result: Value) {
        if let Some(client) = self.clients.get_mut(key) {
            client.respond(id, result);
        }
    }

    /// Sends the document's text if it changed since the server last saw it. Requests call
    /// this first, so a request made right after typing (a trigger character) sees the edit.
    fn send_changes(&mut self, path: &Path, buffer: &Buffer) {
        let Some(doc) = self.docs.get_mut(path) else { return };
        if doc.buffer_version == buffer.version() {
            return;
        }
        doc.buffer_version = buffer.version();
        doc.version += 1;
        let (key, version) = (doc.key.clone(), doc.version);
        if let Some(client) = self.clients.get_mut(&key) {
            client.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": lsp::path_to_uri(path), "version": version },
                    "contentChanges": [{ "text": buffer.text() }],
                }),
            );
        }
    }

    fn request(&mut self, path: &Path, method: &str, mut params: Value, buffer: &Buffer, pos: Pos, pending: Pending) {
        // Inside an HTML file's script, the JavaScript server answers.
        let path = &self.target(path, buffer, pos);
        let capability = match method {
            "textDocument/hover" => "hoverProvider",
            "textDocument/completion" => "completionProvider",
            "textDocument/signatureHelp" => "signatureHelpProvider",
            "textDocument/definition" => "definitionProvider",
            "textDocument/references" => "referencesProvider",
            _ => "",
        };
        if !capability.is_empty() && self.script_doc(path).is_some() && !self.supports_here(path, capability) {
            return;
        }
        self.send_changes(path, buffer);
        self.touch(path);
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return };
        let Some(client) = self.clients.get_mut(&key) else { return };
        let character = client.encoding.to_lsp(&buffer.line(pos.line), pos.col);
        params["textDocument"] = json!({ "uri": lsp::path_to_uri(path) });
        params["position"] = json!({ "line": pos.line, "character": character });
        let id = client.request(method, params);
        self.pending.insert((key, id), pending);
    }

    pub fn hover(&mut self, path: &Path, buffer: &Buffer, pos: Pos) {
        let pending = Pending::Hover { path: path.to_path_buf(), pos };
        self.request(path, "textDocument/hover", json!({}), buffer, pos, pending);
    }

    /// Asks for the call hierarchy item at `pos`; false if the server has no call hierarchy.
    pub fn prepare_call_hierarchy(&mut self, path: &Path, buffer: &Buffer, pos: Pos) -> bool {
        if !self.supports(path, "callHierarchyProvider") {
            return false;
        }
        self.request(path, "textDocument/prepareCallHierarchy", json!({}), buffer, pos, Pending::PrepareCalls);
        true
    }

    /// Asks for the ranges linked to the one at `pos` (tag names that rename together); false
    /// if the server has no linked editing.
    pub fn linked_editing_ranges(&mut self, path: &Path, buffer: &Buffer, pos: Pos, seq: u64) -> bool {
        if !self.supports(path, "linkedEditingRangeProvider") {
            return false;
        }
        self.request(path, "textDocument/linkedEditingRange", json!({}), buffer, pos, Pending::LinkedEditing { seq });
        true
    }

    /// Asks the HTML server what to insert after the character typed before `pos`: an end tag
    /// (`kind` "autoClose") or quotes ("autoQuote").
    pub fn auto_insert(&mut self, path: &Path, buffer: &Buffer, pos: Pos, kind: &str) {
        if self.docs.get(path).is_none_or(|d| d.key.0 != "builtin:html") {
            return;
        }
        let pending = Pending::AutoInsert { path: path.to_path_buf(), version: buffer.version(), pos };
        self.request(path, "html/autoInsert", json!({ "kind": kind }), buffer, pos, pending);
    }

    /// Asks for the type hierarchy item at `pos`; false if the server has no type hierarchy.
    pub fn prepare_type_hierarchy(&mut self, path: &Path, buffer: &Buffer, pos: Pos) -> bool {
        if !self.supports(path, "typeHierarchyProvider") {
            return false;
        }
        self.request(path, "textDocument/prepareTypeHierarchy", json!({}), buffer, pos, Pending::PrepareTypes);
        true
    }

    /// Asks for the supertypes or subtypes of type hierarchy `item` (on `path`'s server).
    pub fn types(&mut self, path: &Path, item: Value, supertypes: bool, node: usize, seq: u64) {
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return };
        let Some(client) = self.clients.get_mut(&key) else { return };
        let method = if supertypes { "typeHierarchy/supertypes" } else { "typeHierarchy/subtypes" };
        let id = client.request(method, json!({ "item": item }));
        self.pending.insert((key, id), Pending::Types { node, supertypes, seq });
    }

    /// Asks for the incoming or outgoing calls of call hierarchy `item` (on `path`'s server).
    pub fn calls(&mut self, path: &Path, item: Value, incoming: bool, node: usize, seq: u64) {
        let Some(key) = self.docs.get(path).map(|d| d.key.clone()) else { return };
        let Some(client) = self.clients.get_mut(&key) else { return };
        let method = if incoming { "callHierarchy/incomingCalls" } else { "callHierarchy/outgoingCalls" };
        let id = client.request(method, json!({ "item": item }));
        self.pending.insert((key, id), Pending::Calls { node, incoming, seq });
    }

    pub fn definition(&mut self, path: &Path, buffer: &Buffer, pos: Pos) {
        self.request(path, "textDocument/definition", json!({}), buffer, pos, Pending::Definition);
    }

    pub fn completion(&mut self, path: &Path, buffer: &Buffer, pos: Pos, trigger: Option<&str>, seq: u64) {
        let context = match trigger {
            Some(t) => json!({ "triggerKind": 2, "triggerCharacter": t }),
            None => json!({ "triggerKind": 1 }),
        };
        let params = json!({ "context": context });
        self.request(path, "textDocument/completion", params, buffer, pos, Pending::Completion { seq });
    }

    /// Text for the status bar describing in-progress server work.
    pub fn progress_text(&self) -> Option<String> {
        let (title, message) = self.progress.values().next()?;
        Some(if message.is_empty() { title.clone() } else { format!("{title}: {message}") })
    }

    pub fn counts(&self) -> (usize, usize) {
        let all = self.diagnostics.values().flat_map(|(_, d)| d);
        all.fold((0, 0), |(e, w), d| match d.severity {
            lsp::Severity::Error => (e + 1, w),
            lsp::Severity::Warning => (e, w + 1),
            _ => (e, w),
        })
    }

    /// Processes everything the servers sent since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut logs = Vec::new();
        let mut exited = Vec::new();
        let mut merged = Vec::new();
        for (key, client) in &mut self.clients {
            let encoding = client.encoding;
            for msg in client.poll() {
                match msg {
                    Incoming::Response { id, result } => {
                        let Some(pending) = self.pending.remove(&(key.clone(), id)) else { continue };
                        let result = match (result, &pending) {
                            (Ok(r), _) => r,
                            (Err(e), Pending::PrepareRename { path, pos }) => {
                                // Servers without prepareRename support answer "method not found":
                                // then any word can be renamed.
                                let unsupported = e.contains("not found") || e.contains("Unhandled method");
                                events.push(Event::PrepareRename {
                                    path: path.clone(),
                                    pos: *pos,
                                    range: None,
                                    error: (!unsupported).then_some(e),
                                    encoding,
                                });
                                continue;
                            }
                            (Err(e), Pending::Format { path, version, save: true }) => {
                                // Save anyway, unformatted.
                                events.push(Event::Formatted { path: path.clone(), version: *version, edits: Vec::new(), encoding, save: true });
                                logs.push(format!("[{}] formatting failed: {e}", key.0));
                                continue;
                            }
                            (Err(_), Pending::SemanticTokens { path, version }) => {
                                let legend = legend_of(client);
                                events.push(Event::SemanticTokens { path: path.clone(), version: *version, data: None, legend, encoding });
                                continue;
                            }
                            (Err(_), Pending::DocumentColors { path, version }) => {
                                events.push(Event::DocumentColors { path: path.clone(), version: *version, colors: None, encoding });
                                continue;
                            }
                            (Err(_), Pending::FoldingRanges { path, version }) => {
                                events.push(Event::FoldingRanges { path: path.clone(), version: *version, ranges: None });
                                continue;
                            }
                            (Err(_), Pending::CodeLenses { path, version }) => {
                                events.push(Event::CodeLenses { path: path.clone(), version: *version, lenses: None, encoding });
                                continue;
                            }
                            (Err(_), Pending::ResolveLens { .. }) => continue,
                            (Err(e), Pending::PrepareTypes) => {
                                events.push(Event::Failed { message: e });
                                continue;
                            }
                            (Err(_), Pending::Types { node, supertypes, seq }) => {
                                events.push(Event::Types { node: *node, supertypes: *supertypes, seq: *seq, items: Vec::new(), encoding });
                                continue;
                            }
                            (Err(e), Pending::PrepareCalls) => {
                                events.push(Event::Failed { message: e });
                                continue;
                            }
                            (Err(_), Pending::Calls { node, incoming, seq }) => {
                                events.push(Event::Calls { node: *node, incoming: *incoming, seq: *seq, calls: Vec::new(), encoding });
                                continue;
                            }
                            (Err(_), Pending::InlayHints { path, version }) => {
                                events.push(Event::InlayHints { path: path.clone(), version: *version, hints: None, encoding });
                                continue;
                            }
                            (Err(_), Pending::Symbols { path, version }) => {
                                // "content modified" while the server loads: asked again later.
                                events.push(Event::Symbols { path: path.clone(), version: *version, symbols: None, encoding });
                                continue;
                            }
                            (Err(e), Pending::Ext) => {
                                events.push(Event::ExtResponse { key: key.clone(), id, result: Err(e) });
                                continue;
                            }
                            (Err(e), Pending::Rename) => {
                                events.push(Event::Failed { message: e });
                                continue;
                            }
                            (Err(_), _) => continue,
                        };
                        match pending {
                            Pending::Hover { path, pos } => {
                                if let Some(markdown) = lsp::parse_hover(&result) {
                                    events.push(Event::Hover { path, pos, markdown });
                                }
                            }
                            Pending::Definition => {
                                events.push(Event::Definition { locations: lsp::parse_locations(&result), encoding });
                            }
                            Pending::Completion { seq } => {
                                let (items, incomplete) = lsp::parse_completions(&result);
                                events.push(Event::Completion { seq, items, incomplete, encoding });
                            }
                            Pending::PrepareRename { path, pos } => {
                                let range = lsp::parse_prepare_rename(&result);
                                // `defaultBehavior: true`: rename the word at the cursor.
                                let default = result["defaultBehavior"].as_bool() == Some(true);
                                let error = (range.is_none() && !default).then(|| "The element can't be renamed.".to_string());
                                events.push(Event::PrepareRename { path, pos, range, error, encoding });
                            }
                            Pending::Format { path, version, save } => {
                                events.push(Event::Formatted { path, version, edits: lsp::parse_text_edits(&result), encoding, save });
                            }
                            Pending::Symbols { path, version } => {
                                // rust-analyzer answers null while it's still loading.
                                let symbols = (!result.is_null()).then(|| lsp::parse_document_symbols(&result));
                                events.push(Event::Symbols { path, version, symbols, encoding });
                            }
                            Pending::SignatureHelp { seq } => {
                                events.push(Event::SignatureHelp { seq, help: lsp::parse_signature_help(&result) });
                            }
                            Pending::SemanticTokens { path, version } => {
                                let data = result["data"].as_array().map(|d| d.iter().map(|n| n.as_u64().unwrap_or(0) as u32).collect());
                                events.push(Event::SemanticTokens { path, version, data, legend: legend_of(client), encoding });
                            }
                            Pending::DocumentColors { path, version } => {
                                let colors = result.as_array().map(|a| {
                                    a.iter()
                                        .filter_map(|c| {
                                            let v = &c["color"];
                                            let n = |k: &str| v[k].as_f64().map(|x| x as f32);
                                            Some((lsp::Range::parse(&c["range"])?, [n("red")?, n("green")?, n("blue")?, n("alpha")?]))
                                        })
                                        .collect()
                                });
                                events.push(Event::DocumentColors { path, version, colors, encoding });
                            }
                            Pending::FoldingRanges { path, version } => {
                                let ranges = result.as_array().map(|a| {
                                    a.iter()
                                        .filter_map(|r| Some((r["startLine"].as_u64()? as usize, r["endLine"].as_u64()? as usize)))
                                        .collect()
                                });
                                events.push(Event::FoldingRanges { path, version, ranges });
                            }
                            Pending::CodeLenses { path, version } => {
                                let lenses = result.as_array().cloned();
                                events.push(Event::CodeLenses { path, version, lenses, encoding });
                            }
                            Pending::PrepareTypes => {
                                events.push(Event::TypeRoots { items: result.as_array().cloned().unwrap_or_default(), encoding });
                            }
                            Pending::Types { node, supertypes, seq } => {
                                events.push(Event::Types { node, supertypes, seq, items: result.as_array().cloned().unwrap_or_default(), encoding });
                            }
                            Pending::PrepareCalls => {
                                events.push(Event::CallRoots { items: result.as_array().cloned().unwrap_or_default(), encoding });
                            }
                            Pending::AutoInsert { path, version, pos } => {
                                if let Some(snippet) = result.as_str() {
                                    events.push(Event::AutoInsert { path, version, pos, snippet: snippet.to_string() });
                                }
                            }
                            Pending::LinkedEditing { seq } => {
                                let ranges = result["ranges"].as_array().map(|a| a.iter().filter_map(lsp::Range::parse).collect());
                                let word_pattern = result["wordPattern"].as_str().map(String::from);
                                events.push(Event::LinkedEditing { seq, ranges, word_pattern, encoding });
                            }
                            Pending::Calls { node, incoming, seq } => {
                                events.push(Event::Calls { node, incoming, seq, calls: result.as_array().cloned().unwrap_or_default(), encoding });
                            }
                            Pending::Ext => events.push(Event::ExtResponse { key: key.clone(), id, result: Ok(result) }),
                            Pending::ResolveLens { path, version, index } => {
                                events.push(Event::LensResolved { path, version, index, lens: result });
                            }
                            Pending::InlayHints { path, version } => {
                                let hints = (!result.is_null()).then(|| lsp::parse_inlay_hints(&result));
                                events.push(Event::InlayHints { path, version, hints, encoding });
                            }
                            Pending::WorkspaceSymbols { seq } => {
                                events.push(Event::WorkspaceSymbols { seq, symbols: lsp::parse_workspace_symbols(&result), encoding });
                            }
                            Pending::CodeActions { auto } => {
                                events.push(Event::CodeActions { actions: lsp::parse_code_actions(&result), encoding, key: key.clone(), auto });
                            }
                            Pending::ResolveAction => {
                                if let Some(action) = lsp::parse_code_actions(&Value::Array(vec![result])).pop() {
                                    events.push(Event::ResolvedAction { action, encoding, key: key.clone() });
                                }
                            }
                            Pending::References => {
                                events.push(Event::References { locations: lsp::parse_locations(&result), encoding });
                            }
                            Pending::Rename => match lsp::parse_workspace_edit(&result) {
                                Some(edit) if !edit.is_empty() => events.push(Event::Edit { edit, encoding, reply: None }),
                                _ => events.push(Event::Failed { message: "No result.".into() }),
                            },
                        }
                    }
                    Incoming::Notification { method, params } => match method.as_str() {
                        "textDocument/publishDiagnostics" => {
                            if let Some((path, diags)) = lsp::parse_diagnostics(&params) {
                                if self.host_of.contains_key(&path) || self.embedded.contains_key(&path) {
                                    merged.push((key.clone(), encoding, path, diags));
                                    continue;
                                }
                                self.published.push(path.clone());
                                if diags.is_empty() {
                                    self.diagnostics.remove(&path);
                                    self.diagnostics_from.remove(&path);
                                } else {
                                    self.diagnostics_from.insert(path.clone(), key.clone());
                                    self.diagnostics.insert(path, (encoding, diags));
                                }
                            }
                        }
                        "workspace/inlayHint/refresh" => self.inlay_refresh = true,
                        "workspace/codeLens/refresh" => self.lens_refresh = true,
                        "$/progress" => {
                            let token = params["token"].to_string();
                            let v = &params["value"];
                            let message = match (v["message"].as_str(), v["percentage"].as_u64()) {
                                (Some(m), Some(p)) => format!("{m} ({p}%)"),
                                (Some(m), None) => m.to_string(),
                                (None, Some(p)) => format!("{p}%"),
                                (None, None) => String::new(),
                            };
                            match v["kind"].as_str() {
                                Some("begin") => {
                                    let title = format!("{}: {}", key.0, v["title"].as_str().unwrap_or(""));
                                    self.progress.insert(token, (title, message));
                                }
                                Some("report") => {
                                    if let Some(entry) = self.progress.get_mut(&token) {
                                        entry.1 = message;
                                    }
                                }
                                _ => {
                                    self.progress.remove(&token);
                                    self.work_done += 1;
                                }
                            }
                        }
                        "window/logMessage" | "window/showMessage" => {
                            if let Some(m) = params["message"].as_str() {
                                logs.push(format!("[{}] {m}", key.0));
                            }
                        }
                        _ => events.push(Event::ExtNotification { key: key.clone(), method, params }),
                    },
                    Incoming::Log(line) => {
                        if client.state != State::Running && says_not_installed(&line) {
                            self.not_installed.insert(key.clone());
                        }
                        logs.push(format!("[{}] {line}", key.0))
                    }
                    Incoming::Exited => exited.push(key.clone()),
                    Incoming::Request { id, method, params } if method == "workspace/applyEdit" => {
                        match lsp::parse_workspace_edit(&params["edit"]) {
                            Some(edit) => events.push(Event::Edit { edit, encoding, reply: Some((key.clone(), id)) }),
                            None => client.respond(id, json!({ "applied": false })),
                        }
                    }
                    Incoming::Request { .. } => {}
                }
            }
        }
        for (key, encoding, path, diags) in merged {
            self.publish_merged(&key, encoding, &path, diags);
        }
        // Places in an HTML file's scripts are places in the file.
        for event in &mut events {
            if let Event::Definition { locations, .. } | Event::References { locations, .. } = event {
                for l in locations {
                    if let Some(host) = self.host_of(&l.path) {
                        l.path = host.to_path_buf();
                    }
                }
            }
        }
        for line in logs {
            self.log(line);
        }
        for key in exited {
            self.log(format!("[{}] exited", key.0));
            self.clients.remove(&key);
            self.docs.retain(|_, d| d.key != key);
            self.last_used.remove(&key);
            self.heavy.remove(&key);
            if self.not_installed.contains(&key) {
                // A stand-in that only says the server is missing (rustup's proxy).
                let lang = Lang::all().find(|l| server_for(*l).is_some_and(|s| s.command == key.0));
                if let Some((lang, server)) = lang.and_then(|l| Some((l, server_for(l)?))) {
                    self.missing.push(MissingServer { language: lang.name(), command: server.command, install: server.install });
                }
            }
            self.failed.insert(key);
        }
        events
    }

    pub fn shutdown(&mut self) {
        for client in self.clients.values_mut() {
            client.shutdown();
        }
        self.clients.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_login_shell_path() {
        // The user's real shell: whatever it prints first, the PATH comes back.
        let path = login_shell_path(Duration::from_secs(10)).expect("no PATH from the login shell");
        assert!(path.contains("/usr/bin"), "{path}");
        assert!(!path.contains(PATH_MARKER));
    }

    #[test]
    fn notices_a_stand_in_for_a_missing_server() {
        assert!(says_not_installed("error: 'rust-analyzer' is not installed for the toolchain 'stable-aarch64-apple-darwin'."));
        assert!(says_not_installed("error: Unknown binary 'rust-analyzer' in official toolchain 'stable-aarch64-apple-darwin'."));
        assert!(!says_not_installed("INFO rust_analyzer: server version 1.90.0 will start"));
    }
}
