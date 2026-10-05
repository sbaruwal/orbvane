//! Debugging, like the standard Run and Debug: breakpoints in the editor's glyph margin (they move
//! with edits and are kept per folder), launch configurations from `.orbvane/launch.json`, and a
//! session with a debug adapter (`dap`: `lldb-dap`, debugpy, Delve's `dlv dap`, and our own
//! Node.js adapter, `jsdebug`). The adapter's
//! answers fill in threads, the call stack, variables (fetched as they're expanded), watch
//! expressions and the Debug Console. `debug_view.rs` draws all of it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use text::{Change, Pos};

use super::{Focus, GitInput, View, Workbench, PANEL_DEBUG_CONSOLE};
use crate::editor::BpLook;
use crate::palette::{Action, InputBox, Item, Palette, Picker};

/// How long to wait for the adapter to answer `disconnect` before stopping it.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// A breakpoint on a line (0-based) of a file.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Breakpoint {
    pub line: usize,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Break only when this expression is true.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub condition: String,
    /// Break only after this many hits (the adapter's syntax, e.g. "3" or ">5").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hit_condition: String,
    /// A logpoint: print this message (with `{expressions}`) instead of breaking.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub log_message: String,
    /// Whether the adapter could set it (None: not sent in this session).
    #[serde(skip)]
    pub verified: Option<bool>,
    /// Why it couldn't be set.
    #[serde(skip)]
    pub message: String,
    #[serde(skip)]
    pub id: Option<i64>,
}

fn yes() -> bool {
    true
}

impl Breakpoint {
    pub fn new(line: usize) -> Self {
        Breakpoint { line, enabled: true, condition: String::new(), hit_condition: String::new(), log_message: String::new(), verified: None, message: String::new(), id: None }
    }
}

/// Which property of a breakpoint an input box edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BpField {
    Condition,
    HitCount,
    LogMessage,
}

/// An entry of a breakpoint's context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BpMenu {
    Add,
    Remove,
    Edit(BpField),
    Enable(bool),
}

/// A choice in the debug pickers.
#[derive(Clone, Debug, PartialEq)]
pub enum DebugPick {
    /// Start the configuration with this name.
    Config(String),
    /// Create `launch.json` with a configuration for this debug type.
    NewConfig(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    /// Started, not yet running the program.
    Starting,
    Running,
    Stopped,
    /// `disconnect` sent; waiting for the adapter.
    Ending,
}

/// An exception breakpoint the adapter offers ("C++ Throw", "Rust Panic").
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ExceptionFilter {
    pub filter: String,
    pub label: String,
    pub enabled: bool,
}

/// A line of the Debug Console.
pub(super) struct ConsoleLine {
    pub text: String,
    /// "stdout", "stderr", "console", "important", or "input" for the user's own lines.
    pub category: String,
    /// A result that can be expanded (the reference of its children).
    pub reference: i64,
}

/// What a request was for, to route its answer.
enum Pending {
    Launch,
    Breakpoints(PathBuf, Vec<usize>),
    ConfigurationDone,
    Threads,
    StackTrace(i64),
    Scopes,
    /// The children of `reference`, shown at tree path `path`.
    Variables(String),
    Watch(usize),
    Repl,
    /// The debug hover over (document, position).
    Hover(usize, Pos),
    Disconnect,
    Other,
}

pub(super) struct Session {
    client: dap::Client,
    pub name: String,
    /// The configuration as started (for Restart).
    config: Value,
    no_debug: bool,
    pub state: State,
    /// Requests in flight, with the stop they belong to (answers from an earlier stop are
    /// dropped).
    pending: HashMap<i64, (Pending, u64)>,
    /// Counts stops and resumes.
    generation: u64,
    pub threads: Vec<dap::Thread>,
    pub stopped: Option<dap::Stopped>,
    /// The thread shown in the call stack and whose frame is focused.
    pub thread: Option<i64>,
    pub frames: Vec<dap::StackFrame>,
    /// The focused frame (index into `frames`).
    pub frame: Option<usize>,
    pub scopes: Vec<dap::Scope>,
    /// Fetched children, by tree path ("scope/Locals", "scope/Locals/p", "watch/0/x").
    pub children: HashMap<String, Vec<dap::Variable>>,
    /// Watch expression results for this stop.
    pub watch_results: Vec<Option<Result<dap::Variable, String>>>,
    /// When `disconnect` was sent.
    ending_since: Option<Instant>,
    /// Whether the session has stopped before (the view opens on the first stop).
    stopped_once: bool,
}

impl Session {
    fn send(&mut self, command: &str, args: Value, pending: Pending) -> i64 {
        let seq = self.client.request(command, args);
        self.pending.insert(seq, (pending, self.generation));
        seq
    }

    /// The focused frame's id.
    pub fn frame_id(&self) -> Option<i64> {
        self.frames.get(self.frame?).map(|f| f.id)
    }

    fn resumed(&mut self) {
        self.state = State::Running;
        self.generation += 1;
        self.stopped = None;
        self.frames.clear();
        self.frame = None;
        self.scopes.clear();
        self.children.clear();
        self.watch_results.clear();
    }
}

pub(super) struct Debug {
    pub breakpoints: BTreeMap<PathBuf, Vec<Breakpoint>>,
    /// Where each file's breakpoints are in its document's edit log: (doc, edit seq).
    synced: HashMap<PathBuf, (usize, u64)>,
    /// Breakpoints are on (Toggle Activate Breakpoints turns them all off).
    pub active: bool,
    pub session: Option<Session>,
    /// A session to start once the current one has ended (Restart).
    restart: Option<(Value, bool)>,
    pub console: Vec<ConsoleLine>,
    /// The Debug Console's input (evaluates in the focused frame).
    pub console_input: crate::widgets::TextField,
    pub console_history: Vec<String>,
    pub console_scroll: f32,
    pub watches: Vec<String>,
    /// The last adapter's exception breakpoints (listed in the Breakpoints section).
    pub exception_filters: Vec<ExceptionFilter>,
    /// Exception filters turned on or off by the user, kept per folder (filter → enabled).
    pub exception_choices: BTreeMap<String, bool>,
    /// Expanded tree paths in the Variables and Watch sections (kept across stops).
    pub expanded: HashSet<String>,
    /// The configuration last started (the view's dropdown).
    pub selected_config: Option<String>,
    /// A session waiting for its preLaunchTask (label, configuration, no-debug).
    pre_launch: Option<(String, Value, bool)>,
    /// Whether the Debug Console has been opened for a session yet.
    console_shown: bool,
    /// A `DebugPlan`'s build running in the background: its result (the program, or why
    /// not) and the configuration to start with it.
    plan_build: Option<(std::sync::mpsc::Receiver<Result<PathBuf, String>>, Value)>,
    pub view: super::debug_view::DebugViewState,
}

impl Default for Debug {
    fn default() -> Self {
        Debug {
            breakpoints: BTreeMap::new(),
            synced: HashMap::new(),
            active: true,
            session: None,
            restart: None,
            console: Vec::new(),
            console_input: Default::default(),
            console_history: Vec::new(),
            console_scroll: 0.0,
            watches: Vec::new(),
            exception_filters: Vec::new(),
            exception_choices: BTreeMap::new(),
            expanded: HashSet::from(["scope/0".to_string()]),
            selected_config: None,
            console_shown: false,
            plan_build: None,
            pre_launch: None,
            view: Default::default(),
        }
    }
}

/// Moves breakpoint lines along with the buffer edits `changes`, like the editor's own
/// decorations: lines inserted or removed above a breakpoint move it; removing its line moves
/// it to where the removal ended; typing a line break at the very start of its line pushes it
/// down with its text.
fn shift(bps: &mut Vec<Breakpoint>, changes: &[Change]) {
    for change in changes {
        let Change::Edit(e) = change else { continue };
        let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
        let delta = new_end as isize - old_end as isize;
        for bp in bps.iter_mut() {
            if bp.line > old_end || (bp.line == start && e.start.1 == 0 && e.old_end == e.start && new_end > start) {
                bp.line = (bp.line as isize + delta).max(0) as usize;
            } else if bp.line > start {
                bp.line = bp.line.min(new_end);
            }
        }
    }
    bps.sort_by_key(|b| b.line);
    bps.dedup_by_key(|b| b.line);
}

/// Replaces `${...}` variables in the strings of a launch configuration.
pub(super) fn substitute(v: &Value, vars: &dyn Fn(&str) -> Option<String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find("${") {
                out.push_str(&rest[..i]);
                let after = &rest[i + 2..];
                match after.find('}') {
                    Some(j) => {
                        let name = &after[..j];
                        match vars(name) {
                            Some(value) => out.push_str(&value),
                            None => out.push_str(&rest[i..i + 2 + j + 1]),
                        }
                        rest = &after[j + 1..];
                    }
                    None => {
                        out.push_str(&rest[i..]);
                        rest = "";
                    }
                }
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, vars)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), substitute(x, vars))).collect()),
        other => other.clone(),
    }
}

/// The debug types we can start, with the name shown in "Select debugger".
const DEBUGGERS: &[(&str, &str)] = &[("lldb-dap", "LLDB DAP"), ("debugpy", "Python Debugger"), ("node", "Node.js"), ("go", "Go")];

/// Go's `"mode": "auto"`: a `_test.go` file is debugged as its package's tests,
/// anything else as a program. Delve wants the package directory for tests. A relative
/// program is in the workspace folder.
fn go_config(config: &mut Value, folder: Option<&Path>) {
    if config["request"] != "launch" {
        return;
    }
    let mut program = config["program"].as_str().unwrap_or(".").to_string();
    if let Some(folder) = folder.filter(|_| !Path::new(&program).is_absolute()) {
        program = folder.join(&program).display().to_string();
        config["program"] = json!(program);
    }
    let mode = config["mode"].as_str().unwrap_or("auto");
    if mode == "auto" || mode == "test" {
        let test = mode == "test" || program.ends_with("_test.go");
        config["mode"] = json!(if test { "test" } else { "debug" });
        if test && program.ends_with(".go") {
            config["program"] = json!(Path::new(&program).parent().map_or(program.clone(), |p| p.display().to_string()));
        }
    }
}

/// `lldb-dap`: from the `lldb-dap.executable-path` setting, the PATH, or Xcode (`xcrun`).
fn lldb_dap(setting: &str) -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    if !setting.is_empty() {
        return Some(PathBuf::from(setting));
    }
    FOUND
        .get_or_init(|| {
            let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("lldb-dap")).find(|p| p.is_file()));
            on_path.or_else(|| {
                let out = std::process::Command::new("xcrun").args(["-f", "lldb-dap"]).output().ok()?;
                let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
                (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
            })
        })
        .clone()
}

impl Workbench {
    // ------------------------------------------------------------ breakpoints

    /// Moves each open file's breakpoints along with its edits. Called every frame.
    pub(super) fn debug_sync_breakpoints(&mut self) {
        let dbg = &mut self.debug;
        for (id, doc) in self.docs.iter().enumerate() {
            let Some(doc) = doc else { continue };
            let Some(path) = doc.buffer.path() else { continue };
            let Some(bps) = dbg.breakpoints.get_mut(path) else { continue };
            let seq = doc.buffer.edit_seq();
            match dbg.synced.get(path) {
                Some(&(d, s)) if d == id && s == seq => {}
                Some(&(d, s)) if d == id => {
                    if let Some(edits) = doc.buffer.edits_since(s) {
                        shift(bps, edits);
                    }
                    let n = doc.buffer.len_lines();
                    bps.retain(|b| b.line < n);
                    dbg.synced.insert(path.to_path_buf(), (id, seq));
                }
                _ => {
                    dbg.synced.insert(path.to_path_buf(), (id, seq));
                }
            }
        }
    }

    /// The breakpoints of `path` as the glyph margin draws them.
    pub(super) fn breakpoint_marks(&self, path: Option<&Path>) -> Vec<(usize, BpLook)> {
        let Some(bps) = path.and_then(|p| self.debug.breakpoints.get(p)) else { return Vec::new() };
        let live = self.debug.session.is_some();
        bps.iter()
            .map(|b| {
                let look = if !b.enabled || !self.debug.active {
                    BpLook::Disabled
                } else if live && b.verified == Some(false) {
                    BpLook::Unverified
                } else if !b.log_message.is_empty() {
                    BpLook::Log
                } else if !b.condition.is_empty() || !b.hit_condition.is_empty() {
                    BpLook::Conditional
                } else {
                    BpLook::Normal
                };
                (b.line, look)
            })
            .collect()
    }

    /// The focused stack frame's line in `path`, and whether it's the top frame.
    pub(super) fn stack_frame_mark(&self, path: Option<&Path>) -> Option<(usize, bool)> {
        let s = self.debug.session.as_ref()?;
        let i = s.frame?;
        let f = s.frames.get(i)?;
        let fp = f.source.as_ref()?.path.as_deref()?;
        (Some(fp) == path && f.line > 0).then(|| (f.line as usize - 1, i == 0))
    }

    fn active_file_line(&self) -> Option<(PathBuf, usize)> {
        let ed = self.active_editor().filter(|e| !e.is_special())?;
        let doc = self.docs[ed.doc].as_ref()?;
        Some((doc.buffer.path()?.to_path_buf(), ed.sel.head.line))
    }

    /// Toggles a breakpoint on `line` of `path` (F9, or a click in the glyph margin).
    pub(super) fn toggle_breakpoint_at(&mut self, path: &Path, line: usize) {
        let bps = self.debug.breakpoints.entry(path.to_path_buf()).or_default();
        match bps.iter().position(|b| b.line == line) {
            Some(i) => {
                bps.remove(i);
            }
            None => {
                let i = bps.partition_point(|b| b.line < line);
                bps.insert(i, Breakpoint::new(line));
            }
        }
        if bps.is_empty() {
            self.debug.breakpoints.remove(path);
        }
        self.send_breakpoints(path);
    }

    pub(super) fn toggle_breakpoint(&mut self) {
        if let Some((path, line)) = self.active_file_line() {
            self.toggle_breakpoint_at(&path, line);
        }
    }

    /// Asks for a breakpoint's condition, hit count or log message (on the cursor's line).
    pub(super) fn edit_breakpoint(&mut self, field: BpField) {
        let Some((path, line)) = self.active_file_line() else { return };
        self.edit_breakpoint_at(path, line, field);
    }

    pub(super) fn edit_breakpoint_at(&mut self, path: PathBuf, line: usize, field: BpField) {
        let bp = self.debug.breakpoints.get(&path).and_then(|b| b.iter().find(|b| b.line == line));
        let (prompt, current) = match field {
            BpField::Condition => ("Break when expression evaluates to true. Press Enter to accept, Escape to cancel.", bp.map(|b| b.condition.clone())),
            BpField::HitCount => ("Break when hit count condition is met. Press Enter to accept, Escape to cancel.", bp.map(|b| b.hit_condition.clone())),
            BpField::LogMessage => ("Message to log when breakpoint is hit. Expressions within {} are interpolated. Press Enter to accept, Escape to cancel.", bp.map(|b| b.log_message.clone())),
        };
        let b = InputBox { prompt: prompt.into(), placeholder: String::new(), purpose: GitInput::Breakpoint(path, line, field), error: None, password: false };
        self.palette = Some(Palette::with_input(b, &current.unwrap_or_default()));
    }

    /// The input box for a breakpoint property was accepted (an empty value clears it; an
    /// empty breakpoint made for it goes away again).
    pub(super) fn set_breakpoint_field(&mut self, path: PathBuf, line: usize, field: BpField, value: String) {
        let bps = self.debug.breakpoints.entry(path.clone()).or_default();
        let i = match bps.iter().position(|b| b.line == line) {
            Some(i) => i,
            None if value.is_empty() => return,
            None => {
                let i = bps.partition_point(|b| b.line < line);
                bps.insert(i, Breakpoint::new(line));
                i
            }
        };
        let bp = &mut bps[i];
        match field {
            BpField::Condition => bp.condition = value,
            BpField::HitCount => bp.hit_condition = value,
            BpField::LogMessage => bp.log_message = value,
        }
        self.send_breakpoints(&path);
    }

    /// The context menu of line `line` in the glyph margin, or of a breakpoint in the
    /// Breakpoints section.
    pub(super) fn breakpoint_menu(&mut self, path: PathBuf, line: usize, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        let bp = self.debug.breakpoints.get(&path).and_then(|b| b.iter().find(|b| b.line == line));
        let item = |label: &str| PopupItem::Item { label: label.into(), enabled: true, checked: None };
        let act = |a: BpMenu| PopupAction::Breakpoint(path.clone(), line, a);
        let entries = match bp {
            Some(bp) => vec![
                (item("Remove Breakpoint"), act(BpMenu::Remove)),
                (item("Edit Condition..."), act(BpMenu::Edit(BpField::Condition))),
                (item("Edit Hit Count..."), act(BpMenu::Edit(BpField::HitCount))),
                (item("Edit Log Message..."), act(BpMenu::Edit(BpField::LogMessage))),
                (PopupItem::Separator, PopupAction::None),
                if bp.enabled { (item("Disable Breakpoint"), act(BpMenu::Enable(false))) } else { (item("Enable Breakpoint"), act(BpMenu::Enable(true))) },
            ],
            None => vec![
                (item("Add Breakpoint"), act(BpMenu::Add)),
                (item("Add Conditional Breakpoint..."), act(BpMenu::Edit(BpField::Condition))),
                (item("Add Logpoint..."), act(BpMenu::Edit(BpField::LogMessage))),
            ],
        };
        self.show_popup(entries, x, y);
    }

    pub(super) fn breakpoint_menu_action(&mut self, path: PathBuf, line: usize, action: BpMenu) {
        match action {
            BpMenu::Add | BpMenu::Remove => self.toggle_breakpoint_at(&path, line),
            BpMenu::Edit(field) => self.edit_breakpoint_at(path, line, field),
            BpMenu::Enable(on) => self.set_breakpoint_enabled(&path, line, on),
        }
    }

    pub(super) fn set_all_breakpoints_enabled(&mut self, enabled: bool) {
        for bp in self.debug.breakpoints.values_mut().flatten() {
            bp.enabled = enabled;
        }
        self.debug.active |= enabled;
        self.send_all_breakpoints();
    }

    pub(super) fn remove_all_breakpoints(&mut self) {
        let paths: Vec<PathBuf> = self.debug.breakpoints.keys().cloned().collect();
        self.debug.breakpoints.clear();
        for p in paths {
            self.send_breakpoints(&p);
        }
    }

    pub(super) fn toggle_breakpoints_active(&mut self) {
        self.debug.active = !self.debug.active;
        self.send_all_breakpoints();
        self.send_exception_filters();
    }

    pub(super) fn set_breakpoint_enabled(&mut self, path: &Path, line: usize, enabled: bool) {
        if let Some(bp) = self.debug.breakpoints.get_mut(path).and_then(|b| b.iter_mut().find(|b| b.line == line)) {
            bp.enabled = enabled;
        }
        self.send_breakpoints(path);
    }

    /// Sends `path`'s breakpoints to the running adapter (all of them, as DAP wants).
    fn send_breakpoints(&mut self, path: &Path) {
        let active = self.debug.active;
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state != State::Starting && s.state != State::Ending) else { return };
        let bps = self.debug.breakpoints.get(path).map(Vec::as_slice).unwrap_or_default();
        let sent: Vec<usize> = bps.iter().filter(|b| b.enabled && active).map(|b| b.line).collect();
        let list: Vec<Value> = bps
            .iter()
            .filter(|b| b.enabled && active)
            .map(|b| {
                let mut v = json!({ "line": b.line + 1 });
                if !b.condition.is_empty() {
                    v["condition"] = json!(b.condition);
                }
                if !b.hit_condition.is_empty() {
                    v["hitCondition"] = json!(b.hit_condition);
                }
                if !b.log_message.is_empty() {
                    v["logMessage"] = json!(b.log_message);
                }
                v
            })
            .collect();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let args = json!({ "source": { "name": name, "path": path }, "breakpoints": list, "sourceModified": false });
        s.send("setBreakpoints", args, Pending::Breakpoints(path.to_path_buf(), sent));
        // Breakpoints not sent (disabled) have no adapter state.
        if let Some(bps) = self.debug.breakpoints.get_mut(path) {
            for b in bps.iter_mut().filter(|b| !(b.enabled && active)) {
                b.verified = None;
                b.id = None;
            }
        }
    }

    fn send_exception_filters(&mut self) {
        let filters: Vec<&str> = self.debug.exception_filters.iter().filter(|f| f.enabled && self.debug.active).map(|f| f.filter.as_str()).collect();
        let args = json!({ "filters": filters });
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state != State::Starting && s.state != State::Ending) else { return };
        if s.client.capabilities["exceptionBreakpointFilters"].is_array() {
            s.send("setExceptionBreakpoints", args, Pending::Other);
        }
    }

    /// Turns an exception breakpoint (by index in the Breakpoints section) on or off.
    pub(super) fn toggle_exception_filter(&mut self, i: usize) {
        let Some(f) = self.debug.exception_filters.get_mut(i) else { return };
        f.enabled = !f.enabled;
        self.debug.exception_choices.insert(f.filter.clone(), f.enabled);
        self.send_exception_filters();
    }

    fn send_all_breakpoints(&mut self) {
        let paths: Vec<PathBuf> = self.debug.breakpoints.keys().cloned().collect();
        for p in paths {
            self.send_breakpoints(&p);
        }
    }

    // ------------------------------------------------------------ configurations

    fn launch_json(&self) -> Option<PathBuf> {
        Some(self.folder()?.join(".orbvane").join("launch.json"))
    }

    /// The configurations in `launch.json` (empty when there's none).
    pub(super) fn launch_configs(&self) -> Result<Vec<Value>, String> {
        let Some(path) = self.launch_json() else { return Ok(Vec::new()) };
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(Vec::new()) };
        let v: Value = serde_json::from_str(&theme::strip_jsonc(&text)).map_err(|e| format!("launch.json: {e}"))?;
        Ok(v["configurations"].as_array().cloned().unwrap_or_default())
    }

    /// The value of a `${variable}` in launch configurations.
    pub(super) fn launch_variable(&self, name: &str) -> Option<String> {
        let folder = self.folder();
        let file = self.active_doc().and_then(|d| d.buffer.path()).map(Path::to_path_buf);
        let s = |p: Option<&Path>| p.map(|p| p.to_string_lossy().into_owned());
        Some(match name {
            "workspaceFolder" | "workspaceRoot" => s(folder.as_deref())?,
            "workspaceFolderBasename" => s(folder.as_deref()?.file_name().map(Path::new))?,
            "file" => s(file.as_deref())?,
            "fileBasename" => s(file.as_deref()?.file_name().map(Path::new))?,
            "fileBasenameNoExtension" => s(file.as_deref()?.file_stem().map(Path::new))?,
            "fileDirname" => s(file.as_deref()?.parent())?,
            "fileExtname" => file.as_deref()?.extension().map(|e| format!(".{}", e.to_string_lossy()))?,
            "relativeFile" => s(file.as_deref()?.strip_prefix(folder.as_deref()?).ok())?,
            "lineNumber" => (self.active_editor()?.sel.head.line + 1).to_string(),
            "cwd" => s(folder.as_deref())?,
            "userHome" => std::env::var("HOME").ok()?,
            "pathSeparator" | "/" => "/".into(),
            _ => std::env::var(name.strip_prefix("env:")?).unwrap_or_default(),
        })
    }

    /// F5: continue when stopped; otherwise start the selected configuration (or ask for a
    /// debugger to create `launch.json` with).
    pub(super) fn debug_start(&mut self, no_debug: bool) {
        match self.debug.session.as_ref().map(|s| s.state) {
            Some(State::Stopped) => return self.debug_continue(),
            Some(_) => return,
            None => {}
        }
        let configs = match self.launch_configs() {
            Ok(c) => c,
            Err(e) => return self.debug_error(&e),
        };
        if configs.is_empty() {
            return self.select_debugger();
        }
        let selected = self.debug.selected_config.as_ref();
        let config = configs.iter().find(|c| c["name"].as_str() == selected.map(String::as_str)).unwrap_or(&configs[0]).clone();
        self.start_session(config, no_debug);
    }

    /// Debug: Select and Start Debugging: a picker of the configurations.
    pub(super) fn select_and_start(&mut self) {
        let configs = match self.launch_configs() {
            Ok(c) => c,
            Err(e) => return self.debug_error(&e),
        };
        if configs.is_empty() {
            return self.select_debugger();
        }
        let choices = configs
            .iter()
            .filter_map(|c| c["name"].as_str())
            .map(|name| Item { label: name.to_string(), detail: String::new(), matches: Vec::new(), shortcut: None, action: Action::Debug(DebugPick::Config(name.to_string())), group: None, kind: None })
            .collect();
        self.palette = Some(Palette::with_picker(Picker { placeholder: "Select and start debug configuration".into(), choices }));
    }

    fn select_debugger(&mut self) {
        if self.folder().is_none() {
            return self.debug_error("Open a folder to create a launch.json file.");
        }
        let choices = DEBUGGERS
            .iter()
            .map(|&(ty, label)| Item { label: label.to_string(), detail: String::new(), matches: Vec::new(), shortcut: None, action: Action::Debug(DebugPick::NewConfig(ty)), group: None, kind: None })
            .collect();
        self.palette = Some(Palette::with_picker(Picker { placeholder: "Select debugger".into(), choices }));
    }

    pub(super) fn debug_pick(&mut self, pick: DebugPick) {
        match pick {
            DebugPick::Config(name) => {
                self.debug.selected_config = Some(name);
                self.debug_start(false);
            }
            DebugPick::NewConfig(ty) => self.create_launch_json(ty),
        }
    }

    /// Debug: Open 'launch.json' (creating it first when there's none).
    pub(super) fn open_launch_json(&mut self) {
        match self.launch_json() {
            Some(p) if p.exists() => self.open_file(&p),
            Some(_) => self.select_debugger(),
            None => self.debug_error("Open a folder to create a launch.json file."),
        }
    }

    /// Writes a `launch.json` with a first configuration for debug type `ty` and opens it.
    /// For a Cargo package the program is its debug build.
    fn create_launch_json(&mut self, ty: &str) {
        let (Some(folder), Some(path)) = (self.folder(), self.launch_json()) else { return };
        // What the first configuration runs: the Cargo package, or package.json's `main`.
        let hint = match ty {
            "node" => std::fs::read_to_string(folder.join("package.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|p| p["main"].as_str().map(|m| m.trim_start_matches("./").to_string())),
            _ => std::fs::read_to_string(folder.join("Cargo.toml")).ok().and_then(|t| cargo_package_name(&t)),
        };
        let text = launch_template(ty, hint.as_deref());
        if !path.exists() {
            if let Err(e) = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, text)) {
                return self.debug_error(&format!("Unable to create 'launch.json' file ({e})."));
            }
        }
        self.open_file(&path);
        self.focus = Focus::Editor;
    }

    fn debug_error(&mut self, msg: &str) {
        if cfg!(test) {
            return self.debug_console(&format!("{msg}\n"), "stderr");
        }
        self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Debug").set_description(msg).show();
    }

    // ------------------------------------------------------------ session

    pub(super) fn start_session(&mut self, config: Value, no_debug: bool) {
        self.start_session_after(config, no_debug, false);
    }

    /// A task finished: if a session was waiting for it, start it, or (when the task failed)
    /// ask.
    pub(super) fn debug_pre_launch_done(&mut self, label: &str, code: Option<i32>) {
        if self.debug.pre_launch.as_ref().is_none_or(|p| p.0 != label) {
            return;
        }
        let (_, config, no_debug) = self.debug.pre_launch.take().unwrap();
        if code == Some(0) {
            return self.start_session_after(config, no_debug, true);
        }
        if cfg!(test) {
            return self.debug_console(&format!("The preLaunchTask '{label}' failed.\n"), "stderr");
        }
        let code = code.map_or("unknown".to_string(), |c| c.to_string());
        let answer = self
            .message_dialog()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Debug")
            .set_description(format!("The preLaunchTask '{label}' terminated with exit code {code}."))
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom("Debug Anyway".into(), "Show Errors".into(), "Abort".into()))
            .show();
        match answer {
            rfd::MessageDialogResult::Custom(b) if b == "Debug Anyway" => self.start_session_after(config, no_debug, true),
            rfd::MessageDialogResult::Custom(b) if b == "Show Errors" => {
                self.panel_visible = true;
                self.panel_tab = super::PANEL_TERMINAL;
            }
            _ => {}
        }
    }

    fn start_session_after(&mut self, config: Value, no_debug: bool, after_task: bool) {
        // One session at a time.
        if self.debug.session.is_some() {
            return self.set_status_message("A debug session is already running.");
        }
        let name = config["name"].as_str().unwrap_or("Debug").to_string();
        let ty = config["type"].as_str().unwrap_or_default().to_string();
        if config["request"].as_str().is_some_and(|r| r != "launch" && r != "attach") {
            return self.debug_error(&format!("Configuration '{name}' has an invalid 'request' value."));
        }
        // The preLaunchTask runs first; `debug_pre_launch_done` starts the session after it.
        if let Some(task) = config["preLaunchTask"].as_str().filter(|_| !after_task) {
            if self.debug.pre_launch.is_some() {
                return;
            }
            if self.run_task(task) {
                self.debug.pre_launch = Some((task.to_string(), config.clone(), no_debug));
            }
            return;
        }
        let vars = |n: &str| self.launch_variable(n);
        let mut config = substitute(&config, &vars);
        let Some(folder) = self.folder() else { return };
        let cwd = config["cwd"].as_str().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| folder.clone());
        let waker = self.waker.clone();
        let started = match ty.as_str() {
            "lldb-dap" | "lldb" => {
                let Some(command) = lldb_dap(&self.settings.string("lldb-dap.executable-path")) else {
                    return self.debug_error("Couldn't find lldb-dap. Install Xcode or LLVM, or set \"lldb-dap.executable-path\".");
                };
                dap::Client::spawn(&command, &[], &cwd, "lldb-dap", waker)
            }
            "debugpy" | "python" => {
                // The adapter runs in the program's Python (a workspace's virtual environment
                // has debugpy, the system's may not).
                let python = match config["python"].as_str() {
                    Some(p) => PathBuf::from(p),
                    None => crate::testing::pytest::interpreter(&folder),
                };
                dap::Client::spawn(&python, &["-m".into(), "debugpy.adapter".into()], &cwd, "debugpy", waker)
            }
            "go" => {
                let Some(dlv) = crate::servers::find_binary("dlv") else {
                    return self.debug_error("Couldn't find dlv. Install Delve (brew install delve, or go install github.com/go-delve/delve/cmd/dlv@latest).");
                };
                go_config(&mut config, self.folder().as_deref());
                if config["request"] == "attach" && config["mode"] == "remote" {
                    let addr = format!("{}:{}", config["host"].as_str().unwrap_or("127.0.0.1"), config["port"].as_u64().unwrap_or(2345));
                    dap::Client::connect(&addr, "go", waker)
                } else {
                    dap::Client::spawn_client_addr(&dlv, &["dap".into(), "--client-addr={addr}".into()], &cwd, "go", waker)
                }
            }
            "node" | "pwa-node" => {
                // A bare runtime name is looked up like a language server's.
                let runtime = config["runtimeExecutable"].as_str().unwrap_or("node").to_string();
                if !runtime.contains('/') {
                    match crate::servers::find_binary(&runtime) {
                        Some(p) => config["runtimeExecutable"] = json!(p),
                        None => return self.debug_error(&format!("Can't find Node.js binary \"{runtime}\": path does not exist. Make sure Node.js is installed and in your PATH, or set the \"runtimeExecutable\" in your launch.json")),
                    }
                }
                dap::Client::in_process("node", waker, jsdebug::serve)
            }
            _ => return self.debug_error(&format!("Configured debug type '{ty}' is not supported.")),
        };
        let client = match started {
            Ok(c) => c,
            Err(e) => return self.debug_error(&format!("Couldn't start the debug adapter for '{ty}': {e}")),
        };
        self.debug.selected_config = Some(name.clone());
        self.debug.console.clear();
        self.debug.console_scroll = 0.0;
        for bp in self.debug.breakpoints.values_mut().flatten() {
            bp.verified = None;
            bp.id = None;
        }
        let mut session = Session {
            client,
            name,
            config,
            no_debug,
            state: State::Starting,
            pending: HashMap::new(),
            generation: 0,
            threads: Vec::new(),
            stopped: None,
            thread: None,
            frames: Vec::new(),
            frame: None,
            scopes: Vec::new(),
            children: HashMap::new(),
            watch_results: Vec::new(),
            ending_since: None,
            stopped_once: false,
        };
        // `Client::spawn` sent `initialize` as request 1.
        session.pending.insert(1, (Pending::Other, 0));
        self.debug.session = Some(session);
        // The Debug Console opens for the first session (`debug.internalConsoleOptions`).
        if !self.debug.console_shown {
            self.debug.console_shown = true;
            self.panel_visible = true;
            self.panel_tab = PANEL_DEBUG_CONSOLE;
        }
    }

    fn debug_console(&mut self, text: &str, category: &str) {
        let c = &mut self.debug.console;
        // Output arrives in pieces; a piece without a line break continues the last line.
        let mut parts = text.split('\n').peekable();
        if let Some(first) = parts.next() {
            match c.last_mut() {
                Some(last) if last.category == category && !last.text.ends_with('\n') && last.reference == 0 => last.text.push_str(first),
                _ => c.push(ConsoleLine { text: first.to_string(), category: category.into(), reference: 0 }),
            }
        }
        for part in parts {
            if let Some(last) = c.last_mut() {
                last.text.push('\n');
            }
            if !part.is_empty() {
                c.push(ConsoleLine { text: part.to_string(), category: category.into(), reference: 0 });
            }
        }
        const MAX: usize = 10_000;
        if c.len() > MAX {
            c.drain(..c.len() - MAX);
        }
    }

    /// Debugs what `plan` describes (a Debug code lens, a test): builds it in the background,
    /// then starts its configuration on the program the build made.
    pub(super) fn start_debug_plan(&mut self, plan: crate::runnables::DebugPlan) {
        if self.debug.plan_build.is_some() || self.debug.session.is_some() {
            self.set_status_message("A debug session is already running.");
            return;
        }
        let Some(build) = plan.build else { return self.start_session(plan.config, false) };
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        let (dir, program) = (plan.dir, plan.program);
        std::thread::spawn(move || {
            let _ = tx.send(crate::runnables::build(&build, &dir, program));
            waker();
        });
        self.set_status_message(&format!("Building {} for debugging...", plan.label));
        self.debug.plan_build = Some((rx, plan.config));
    }

    fn poll_debug_plan(&mut self) {
        let Some((rx, _)) = &self.debug.plan_build else { return };
        let Ok(result) = rx.try_recv() else { return };
        let (_, mut config) = self.debug.plan_build.take().unwrap();
        match result {
            Ok(program) => {
                config["program"] = json!(program);
                self.start_session(config, false);
            }
            Err(e) => {
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Build failed").set_description(e).show();
            }
        }
    }

    /// Handles the adapter's messages. Called every frame.
    pub(super) fn debug_tick(&mut self) {
        self.debug_sync_breakpoints();
        self.poll_debug_plan();
        let Some(s) = self.debug.session.as_mut() else { return };
        if s.ending_since.is_some_and(|t| t.elapsed() > DISCONNECT_TIMEOUT) {
            s.client.kill();
        }
        let messages = s.client.poll();
        for msg in messages {
            match msg {
                dap::Incoming::Response { request_seq, command, result } => self.debug_response(request_seq, &command, result),
                dap::Incoming::Event { event, body } => self.debug_event(&event, &body),
                dap::Incoming::Request { seq, command, .. } => {
                    // runInTerminal / startDebugging aren't offered; say so.
                    if let Some(s) = self.debug.session.as_mut() {
                        s.client.respond(seq, &command, false, json!({ "error": { "id": 1, "format": "Not supported" } }));
                    }
                }
                dap::Incoming::Log(line) => self.debug_console(&format!("{line}\n"), "stderr"),
                dap::Incoming::Exited => {}
            }
            if self.debug.session.is_none() {
                return;
            }
        }
        if self.debug.session.as_ref().is_some_and(|s| s.client.exited()) {
            self.end_session();
        }
    }

    /// When the session needs attention without input (a stuck `disconnect`).
    pub(super) fn debug_deadline(&self) -> Option<Instant> {
        self.debug.session.as_ref()?.ending_since.map(|t| t + DISCONNECT_TIMEOUT)
    }

    fn end_session(&mut self) {
        let Some(mut s) = self.debug.session.take() else { return };
        s.client.kill();
        for bp in self.debug.breakpoints.values_mut().flatten() {
            bp.verified = None;
            bp.id = None;
        }
        if let Some((config, no_debug)) = self.debug.restart.take() {
            self.start_session(config, no_debug);
        }
    }

    fn debug_response(&mut self, seq: i64, command: &str, result: Result<Value, String>) {
        let Some(s) = self.debug.session.as_mut() else { return };
        let Some((pending, generation)) = s.pending.remove(&seq) else { return };
        if command == "initialize" {
            if let Err(e) = result {
                self.debug_console(&format!("{e}\n"), "stderr");
                return self.debug_error(&e);
            }
            let choices = &self.debug.exception_choices;
            self.debug.exception_filters = s.client.capabilities["exceptionBreakpointFilters"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|f| {
                            let filter = f["filter"].as_str()?.to_string();
                            let enabled = choices.get(&filter).copied().unwrap_or(f["default"].as_bool().unwrap_or(false));
                            Some(ExceptionFilter { label: f["label"].as_str().unwrap_or(&filter).to_string(), filter, enabled })
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Launch (or attach) with the configuration as the adapter's arguments.
            let mut args = s.config.clone();
            if s.no_debug {
                args["noDebug"] = json!(true);
            }
            let request = s.config["request"].as_str().unwrap_or("launch").to_string();
            s.send(&request, args, Pending::Launch);
            return;
        }
        let stale = generation != s.generation;
        match pending {
            Pending::Launch => {
                if let Err(e) = result {
                    self.debug_console(&format!("{e}\n"), "stderr");
                    self.debug_error(&e);
                    return self.debug_stop();
                }
            }
            Pending::Breakpoints(path, lines) => {
                let Ok(body) = result else { return };
                let answers = dap::Breakpoint::parse_all(&body);
                if let Some(bps) = self.debug.breakpoints.get_mut(&path) {
                    for (line, a) in lines.iter().zip(answers) {
                        let Some(bp) = bps.iter_mut().find(|b| b.line == *line) else { continue };
                        bp.verified = Some(a.verified);
                        bp.message = a.message;
                        bp.id = a.id;
                        // The adapter may move a breakpoint to the next line with code.
                        if let Some(l) = a.line.filter(|&l| l >= 1 && a.verified) {
                            bp.line = l as usize - 1;
                        }
                    }
                    bps.sort_by_key(|b| b.line);
                    bps.dedup_by_key(|b| b.line);
                }
            }
            Pending::ConfigurationDone => {
                if let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Starting) {
                    s.state = State::Running;
                }
            }
            Pending::Threads => {
                let Ok(body) = result else { return };
                s.threads = dap::Thread::parse_all(&body);
                // lldb-dap can leave out the thread that just stopped (a test's thread): it's
                // listed anyway.
                if let Some(t) = s.thread.filter(|t| !s.threads.iter().any(|x| x.id == *t)) {
                    s.threads.push(dap::Thread { id: t, name: format!("Thread #{t}") });
                }
                if s.thread.is_none() {
                    s.thread = s.threads.first().map(|t| t.id);
                }
            }
            Pending::StackTrace(thread) if !stale => {
                let Ok(body) = result else { return };
                if s.thread != Some(thread) {
                    return;
                }
                s.frames = dap::StackFrame::parse_all(&body).0;
                // Focus the top frame with source.
                let i = s.frames.iter().position(|f| f.source.as_ref().is_some_and(|src| src.path.is_some()) && f.hint != "subtle").unwrap_or(0);
                self.focus_frame(i);
            }
            Pending::Scopes if !stale => {
                let Ok(body) = result else { return };
                s.scopes = dap::Scope::parse_all(&body);
                let scopes = s.scopes.clone();
                for (i, scope) in scopes.iter().enumerate() {
                    let path = format!("scope/{i}");
                    if self.debug.expanded.contains(&path) {
                        self.fetch_children(scope.reference, path);
                    }
                }
            }
            Pending::Variables(path) if !stale => {
                let vars = match result {
                    Ok(body) => dap::Variable::parse_all(&body),
                    Err(e) => vec![dap::Variable { name: String::new(), value: e, ty: String::new(), reference: 0, evaluate_name: String::new() }],
                };
                // Fetch the children of what was expanded at the last stop too.
                let open: Vec<(i64, String)> =
                    vars.iter().filter(|v| v.reference > 0).map(|v| (v.reference, format!("{path}/{}", v.name))).filter(|(_, p)| self.debug.expanded.contains(p)).collect();
                s.children.insert(path, vars);
                for (reference, path) in open {
                    self.fetch_children(reference, path);
                }
            }
            Pending::Watch(i) if !stale => {
                let expr = self.debug.watches.get(i).cloned().unwrap_or_default();
                let value = result.map(|body| dap::Variable::from_evaluate(&expr, &body));
                if s.watch_results.len() <= i {
                    s.watch_results.resize(i + 1, None);
                }
                let open = value.as_ref().ok().filter(|v| v.reference > 0).map(|v| (v.reference, format!("watch/{i}")));
                s.watch_results[i] = Some(value);
                if let Some((reference, path)) = open.filter(|(_, p)| self.debug.expanded.contains(p)) {
                    self.fetch_children(reference, path);
                }
            }
            Pending::Hover(doc, pos) if !stale => {
                let value = result.map(|body| body["result"].as_str().unwrap_or_default().to_string());
                self.debug_hover_arrived(doc, pos, value);
            }
            Pending::Repl => match result {
                Ok(body) => {
                    let v = dap::Variable::from_evaluate("", &body);
                    self.debug.console.push(ConsoleLine { text: v.value, category: "result".into(), reference: v.reference });
                }
                Err(e) => self.debug_console(&format!("{e}\n"), "stderr"),
            },
            Pending::Disconnect => self.end_session(),
            _ => {}
        }
    }

    fn debug_event(&mut self, event: &str, body: &Value) {
        let Some(s) = self.debug.session.as_mut() else { return };
        match event {
            "initialized" => {
                // Configuration: breakpoints, exception filters, then configurationDone.
                s.state = State::Running;
                self.send_all_breakpoints();
                self.send_exception_filters();
                let Some(s) = self.debug.session.as_mut() else { return };
                if s.client.supports("supportsConfigurationDoneRequest") {
                    s.send("configurationDone", Value::Null, Pending::ConfigurationDone);
                }
            }
            "stopped" => {
                let stopped = dap::Stopped::parse(body);
                s.state = State::Stopped;
                s.generation += 1;
                s.thread = stopped.thread.or(s.thread);
                s.stopped = Some(stopped);
                s.frames.clear();
                s.frame = None;
                s.scopes.clear();
                s.children.clear();
                s.send("threads", Value::Null, Pending::Threads);
                if let Some(t) = s.thread {
                    s.send("stackTrace", json!({ "threadId": t, "startFrame": 0, "levels": 100 }), Pending::StackTrace(t));
                }
                // The Run and Debug view opens on the first stop (`debug.openDebug`).
                if !s.stopped_once {
                    s.stopped_once = true;
                    self.view = View::Debug;
                    self.sidebar_visible = true;
                }
            }
            "continued" => {
                if s.state == State::Stopped {
                    s.resumed();
                }
            }
            "output" => {
                let out = dap::Output::parse(body);
                if out.category != "telemetry" {
                    self.debug_console(&out.text, &out.category);
                }
            }
            "breakpoint" => {
                let bp = dap::Breakpoint::parse(&body["breakpoint"]);
                let Some(id) = bp.id else { return };
                for b in self.debug.breakpoints.values_mut().flatten().filter(|b| b.id == Some(id)) {
                    b.verified = Some(bp.verified);
                    b.message = bp.message.clone();
                    if let Some(l) = bp.line.filter(|&l| l >= 1) {
                        b.line = l as usize - 1;
                    }
                }
            }
            "thread" => {
                if s.state == State::Stopped {
                    s.send("threads", Value::Null, Pending::Threads);
                }
            }
            "invalidated" => {
                if s.state == State::Stopped {
                    if let Some(i) = s.frame {
                        self.focus_frame(i);
                    }
                }
            }
            "terminated" => {
                if body["restart"].as_bool().unwrap_or(false) {
                    self.debug.restart = Some((s.config.clone(), s.no_debug));
                }
                self.debug_stop();
            }
            _ => {}
        }
    }

    /// Focuses frame `i` of the call stack: shows its line and fetches its scopes.
    pub(super) fn focus_frame(&mut self, i: usize) {
        let Some(s) = self.debug.session.as_mut() else { return };
        let Some(frame) = s.frames.get(i).cloned() else { return };
        s.frame = Some(i);
        s.scopes.clear();
        s.children.clear();
        s.watch_results.clear();
        s.send("scopes", json!({ "frameId": frame.id }), Pending::Scopes);
        self.evaluate_watches();
        if let Some(path) = frame.source.as_ref().and_then(|src| src.path.clone()).filter(|p| p.exists()) {
            let pos = Pos::new((frame.line.max(1) - 1) as usize, (frame.column.max(1) - 1) as usize);
            self.open_file(&path);
            if let Some((ed, doc)) = self.active_mut() {
                ed.reveal_at(doc, pos);
            }
        }
    }

    /// Asks for the focused thread's call stack (after picking another thread).
    pub(super) fn debug_request_stack(&mut self) {
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Stopped) else { return };
        let Some(t) = s.thread else { return };
        s.send("stackTrace", json!({ "threadId": t, "startFrame": 0, "levels": 100 }), Pending::StackTrace(t));
    }

    /// Asks for the children of `reference`, shown at tree `path`.
    pub(super) fn fetch_children(&mut self, reference: i64, path: String) {
        let Some(s) = self.debug.session.as_mut() else { return };
        s.send("variables", json!({ "variablesReference": reference }), Pending::Variables(path));
    }

    /// Expands or collapses a node of the Variables or Watch tree.
    pub(super) fn toggle_variable(&mut self, path: String, reference: i64) {
        if self.debug.expanded.remove(&path) {
            return;
        }
        self.debug.expanded.insert(path.clone());
        let fetched = self.debug.session.as_ref().is_some_and(|s| s.children.contains_key(&path));
        if reference > 0 && !fetched {
            self.fetch_children(reference, path);
        }
    }

    pub(super) fn evaluate_watches(&mut self) {
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Stopped) else { return };
        let frame = s.frame_id();
        s.watch_results = vec![None; self.debug.watches.len()];
        for (i, expr) in self.debug.watches.iter().enumerate() {
            let mut args = json!({ "expression": expr, "context": "watch" });
            if let Some(f) = frame {
                args["frameId"] = json!(f);
            }
            s.send("evaluate", args, Pending::Watch(i));
        }
    }

    /// Debug: Add to Watch: the editor's selection (or the word at the cursor), else ask.
    pub(super) fn add_watch_selection(&mut self) {
        let expr = self.active_editor().zip(self.active_doc()).and_then(|(ed, doc)| {
            let sel = if ed.sel.is_empty() { doc.buffer.word_at(ed.sel.head) } else { ed.sel };
            let text = doc.buffer.text_in(&sel);
            Some(text.trim().to_string()).filter(|t| !t.is_empty() && !t.contains('\n'))
        });
        match expr {
            Some(expr) => {
                self.set_watch(None, expr);
                self.view = View::Debug;
                self.sidebar_visible = true;
            }
            None => self.add_watch_prompt(),
        }
    }

    /// Evaluates the expression under the mouse for the hover, when stopped. Returns whether
    /// it asked.
    pub(super) fn debug_hover(&mut self, doc_id: usize, pos: Pos) -> bool {
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Stopped) else { return false };
        let Some(doc) = self.docs[doc_id].as_ref() else { return false };
        let Some(expr) = hover_expression(&doc.buffer.line(pos.line), pos.col) else { return false };
        let mut args = json!({ "expression": expr, "context": "hover" });
        if let Some(f) = s.frame_id() {
            args["frameId"] = json!(f);
        }
        s.send("evaluate", args, Pending::Hover(doc_id, pos));
        true
    }

    pub(super) fn add_watch_prompt(&mut self) {
        let b = InputBox { prompt: "Expression to watch".into(), placeholder: String::new(), purpose: GitInput::Watch(None), error: None, password: false };
        self.palette = Some(Palette::with_input(b, ""));
    }

    pub(super) fn edit_watch(&mut self, i: usize) {
        let current = self.debug.watches.get(i).cloned().unwrap_or_default();
        let b = InputBox { prompt: "Expression to watch".into(), placeholder: String::new(), purpose: GitInput::Watch(Some(i)), error: None, password: false };
        self.palette = Some(Palette::with_input(b, &current));
    }

    /// The watch input box was accepted: add (or change, or with an empty value remove) it.
    pub(super) fn set_watch(&mut self, index: Option<usize>, expr: String) {
        match index {
            Some(i) if expr.is_empty() => {
                if i < self.debug.watches.len() {
                    self.debug.watches.remove(i);
                }
            }
            Some(i) => {
                if let Some(w) = self.debug.watches.get_mut(i) {
                    *w = expr;
                }
            }
            None if expr.is_empty() => return,
            None => self.debug.watches.push(expr),
        }
        self.debug.expanded.retain(|p| !p.starts_with("watch/"));
        self.evaluate_watches();
    }

    pub(super) fn remove_watch(&mut self, i: usize) {
        self.set_watch(Some(i), String::new());
    }

    /// Enter in the Debug Console: evaluate the input in the focused frame.
    pub(super) fn debug_console_submit(&mut self) {
        let expr = self.debug.console_input.text.trim().to_string();
        if expr.is_empty() {
            return;
        }
        self.debug.console_input.set_text("");
        self.debug.console_history.retain(|h| *h != expr);
        self.debug.console_history.push(expr.clone());
        self.debug.console.push(ConsoleLine { text: expr.clone(), category: "input".into(), reference: 0 });
        let Some(s) = self.debug.session.as_mut() else {
            return self.debug_console("No active debug session.\n", "stderr");
        };
        let mut args = json!({ "expression": expr, "context": "repl" });
        if let Some(f) = s.frame_id() {
            args["frameId"] = json!(f);
        }
        s.send("evaluate", args, Pending::Repl);
    }

    // ------------------------------------------------------------ run control

    fn thread_request(&mut self, command: &str) {
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Stopped) else { return };
        let Some(thread) = s.thread else { return };
        s.send(command, json!({ "threadId": thread }), Pending::Other);
        s.resumed();
    }

    pub(super) fn debug_continue(&mut self) {
        self.thread_request("continue");
    }

    pub(super) fn debug_step(&mut self, command: &str) {
        self.thread_request(command);
    }

    pub(super) fn debug_pause(&mut self) {
        let Some(s) = self.debug.session.as_mut().filter(|s| s.state == State::Running) else { return };
        let thread = s.thread.or(s.threads.first().map(|t| t.id)).unwrap_or(0);
        s.send("pause", json!({ "threadId": thread }), Pending::Other);
    }

    /// Stop (⇧F5): ends the session, terminating the program it launched.
    pub(super) fn debug_stop(&mut self) {
        let Some(s) = self.debug.session.as_mut() else { return };
        if s.state == State::Ending {
            return;
        }
        s.state = State::Ending;
        s.ending_since = Some(Instant::now());
        let launched = s.config["request"].as_str() != Some("attach");
        s.send("disconnect", json!({ "restart": false, "terminateDebuggee": launched }), Pending::Disconnect);
    }

    /// Restart (⇧⌘F5): stop, then start the same configuration again.
    pub(super) fn debug_restart(&mut self) {
        let Some(s) = self.debug.session.as_ref() else { return self.debug_start(false) };
        self.debug.restart = Some((s.config.clone(), s.no_debug));
        self.debug_stop();
    }

    /// Stops the adapter for good (quitting).
    pub(super) fn debug_shutdown(&mut self) {
        if let Some(mut s) = self.debug.session.take() {
            let launched = s.config["request"].as_str() != Some("attach");
            s.client.request("disconnect", json!({ "terminateDebuggee": launched }));
            std::thread::sleep(Duration::from_millis(100));
            s.client.kill();
        }
    }

    pub(super) fn debug_focus_console(&mut self) {
        self.panel_visible = true;
        self.panel_tab = PANEL_DEBUG_CONSOLE;
        self.focus = Focus::DebugConsole;
    }
}

/// The expression to evaluate when hovering column `col` of `line`: the word there with the
/// member chain before it (`self.width` over `width`), like the standard default. None off a word.
fn hover_expression(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if !chars.get(col).is_some_and(|&c| word(c)) {
        return None;
    }
    let mut end = col;
    while end < chars.len() && word(chars[end]) {
        end += 1;
    }
    let mut start = col;
    loop {
        while start > 0 && word(chars[start - 1]) {
            start -= 1;
        }
        // Continue left over `.` or `->` to the object the member belongs to.
        if start >= 2 && chars[start - 1] == '.' && word(chars[start - 2]) {
            start -= 1;
        } else if start >= 3 && chars[start - 1] == '>' && chars[start - 2] == '-' && word(chars[start - 3]) {
            start -= 2;
        } else {
            break;
        }
    }
    let expr: String = chars[start..end].iter().collect();
    (!expr.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(expr)
}

/// A new `launch.json` with a first configuration for debug type `ty`, written as text so the
/// keys keep the order. `package` is the Cargo package (debugged from its debug build),
/// or for Node the program in package.json's `main` (else the current file).
fn launch_template(ty: &str, package: Option<&str>) -> String {
    let fields: Vec<(&str, String)> = match ty {
        "node" => vec![
            ("type", "\"node\"".into()),
            ("request", "\"launch\"".into()),
            ("name", "\"Launch Program\"".into()),
            ("skipFiles", "[\n                \"<node_internals>/**\"\n            ]".into()),
            ("program", serde_json::to_string(&package.map_or("${file}".to_string(), |m| format!("${{workspaceFolder}}/{m}"))).unwrap()),
        ],
        "go" => vec![
            ("name", "\"Launch Package\"".into()),
            ("type", "\"go\"".into()),
            ("request", "\"launch\"".into()),
            ("mode", "\"auto\"".into()),
            ("program", "\"${fileDirname}\"".into()),
        ],
        "debugpy" => vec![
            ("type", "\"debugpy\"".into()),
            ("request", "\"launch\"".into()),
            ("name", "\"Python Debugger: Current File\"".into()),
            ("program", "\"${file}\"".into()),
            ("console", "\"internalConsole\"".into()),
        ],
        _ => {
            let program = match package {
                Some(name) => format!("${{workspaceFolder}}/target/debug/{name}"),
                None => "${workspaceFolder}/<your program>".into(),
            };
            let name = package.map_or("Launch".to_string(), |n| format!("Debug {n}"));
            let mut fields = vec![
                ("type", "\"lldb-dap\"".into()),
                ("request", "\"launch\"".into()),
                ("name", serde_json::to_string(&name).unwrap()),
                ("program", serde_json::to_string(&program).unwrap()),
                ("args", "[]".into()),
                ("cwd", "\"${workspaceFolder}\"".into()),
            ];
            // Build first, with the detected Cargo task.
            if package.is_some() {
                fields.push(("preLaunchTask", "\"rust: cargo build\"".into()));
            }
            fields
        }
    };
    let body = fields.iter().map(|(k, v)| format!("            \"{k}\": {v}")).collect::<Vec<_>>().join(",\n");
    format!(
        "{{\n    // Use IntelliSense to learn about possible attributes.\n    // Hover to view descriptions of existing attributes.\n    \"version\": \"0.2.0\",\n    \"configurations\": [\n        {{\n{body}\n        }}\n    ]\n}}\n"
    )
}

/// The `[package]` name in a Cargo.toml.
fn cargo_package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some(rest) = line.strip_prefix("name") {
                let value = rest.trim_start().strip_prefix('=')?.trim();
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::{Buffer, EditKind, Selection};

    fn lines(bps: &[Breakpoint]) -> Vec<usize> {
        bps.iter().map(|b| b.line).collect()
    }

    fn edit(b: &mut Buffer, from: Pos, to: Pos, text: &str) {
        let sel = [Selection::caret(from)];
        b.edit(&sel, &[(from, to, text)], EditKind::Other);
    }

    #[test]
    fn breakpoints_move_with_edits() {
        let mut b = Buffer::new();
        b.insert(Selection::default(), "a\nb\nc\nd\ne\n");
        let mut bps = vec![Breakpoint::new(2), Breakpoint::new(4)];
        let seq = b.edit_seq();
        // Two lines inserted above both.
        edit(&mut b, Pos::new(0, 0), Pos::new(0, 0), "x\ny\n");
        shift(&mut bps, b.edits_since(seq).unwrap());
        assert_eq!(lines(&bps), [4, 6]);
        // Removing the first breakpoint's line moves it to where the removal ended.
        let seq = b.edit_seq();
        edit(&mut b, Pos::new(3, 0), Pos::new(5, 0), "");
        shift(&mut bps, b.edits_since(seq).unwrap());
        assert_eq!(lines(&bps), [3, 4]);
        // A line break typed at the start of a breakpoint's line pushes it down.
        let seq = b.edit_seq();
        edit(&mut b, Pos::new(4, 0), Pos::new(4, 0), "\n");
        shift(&mut bps, b.edits_since(seq).unwrap());
        assert_eq!(lines(&bps), [3, 5]);
        // Typing within the line keeps it.
        let seq = b.edit_seq();
        edit(&mut b, Pos::new(3, 1), Pos::new(3, 1), "zz");
        shift(&mut bps, b.edits_since(seq).unwrap());
        assert_eq!(lines(&bps), [3, 5]);
    }

    #[test]
    fn substitutes_variables() {
        let vars = |n: &str| match n {
            "workspaceFolder" => Some("/w".to_string()),
            "env:HOME" => Some("/h".to_string()),
            _ => None,
        };
        let v = json!({ "program": "${workspaceFolder}/target/debug/x", "args": ["${env:HOME}", "${unknown}", "${"], "n": 1 });
        let out = substitute(&v, &vars);
        assert_eq!(out["program"], "/w/target/debug/x");
        assert_eq!(out["args"], json!(["/h", "${unknown}", "${"]));
        assert_eq!(out["n"], 1);
    }

    /// Runs `step` until `done` holds, polling the adapter (fails after 10 seconds).
    fn wait_for(wb: &mut Workbench, what: &str, done: impl Fn(&Workbench) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(wb) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; console: {:?}", wb.debug.console.iter().map(|l| &l.text).collect::<Vec<_>>());
            wb.debug_tick();
            wb.tasks_tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A folder with a 20-line file of no language (so no language server starts), the fake
    /// adapter, `launch` as its launch.json and `tasks` as its tasks.json.
    fn demo_folder(tag: &str, launch: &str, tasks: &str) -> (Workbench, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbvane-debug-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".orbvane")).unwrap();
        let file = dir.join("prog.demo");
        std::fs::write(&file, (1..=20).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let adapter = dir.join("fake_dap.py");
        std::fs::write(&adapter, include_str!("../../testdata/fake_dap.py")).unwrap();
        std::fs::set_permissions(&adapter, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::write(dir.join(".orbvane/launch.json"), launch).unwrap();
        if !tasks.is_empty() {
            std::fs::write(dir.join(".orbvane/tasks.json"), tasks).unwrap();
        }
        std::fs::write(dir.join(".orbvane/settings.json"), format!("{{ \"lldb-dap.executable-path\": {:?} }}", adapter.to_string_lossy())).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        (wb, dir, file)
    }

    /// A whole session against `testdata/fake_dap.py`: breakpoints, a stop, variables, watches,
    /// the console, stepping and the end of the program.
    #[test]
    fn a_session_against_a_fake_adapter() {
        let launch = r#"{ "version": "0.2.0", "configurations": [
            { "type": "lldb-dap", "request": "launch", "name": "Demo", "program": "${workspaceFolder}/prog" } ] }"#;
        let (mut wb, dir, file) = demo_folder("session", launch, "");

        wb.toggle_breakpoint_at(&file, 6);
        wb.toggle_breakpoint_at(&file, 19); // past the fake program's code: unverified
        wb.debug_start(false);
        wait_for(&mut wb, "the first stop", |wb| wb.debug.session.as_ref().is_some_and(|s| s.state == State::Stopped && s.children.contains_key("scope/0")));
        let s = wb.debug.session.as_ref().unwrap();
        assert_eq!(s.frames[0].name, "dbgdemo::add");
        assert_eq!(s.threads.len(), 2);
        assert_eq!(wb.stack_frame_mark(Some(&file)), Some((6, true)));
        assert_eq!(wb.breakpoint_marks(Some(&file)), [(6, BpLook::Normal), (19, BpLook::Unverified)]);
        let locals: Vec<&str> = s.children["scope/0"].iter().map(|v| v.name.as_str()).collect();
        assert_eq!(locals, ["a", "b", "p", "names", "ok"]);
        // The adapter's exception breakpoints, on by default as it says.
        assert_eq!(wb.debug.exception_filters, [ExceptionFilter { filter: "rust_panic".into(), label: "Rust Panic".into(), enabled: true }]);
        // Registers are "expensive": not fetched until expanded.
        assert!(!s.children.contains_key("scope/1"));

        // Expanding a struct fetches its fields.
        wb.toggle_variable("scope/0/p".into(), 77);
        wait_for(&mut wb, "p's fields", |wb| wb.debug.session.as_ref().unwrap().children.contains_key("scope/0/p"));
        // Watches and the console evaluate in the focused frame.
        wb.set_watch(None, "a + b".into());
        wb.set_watch(None, "nope".into());
        wb.debug.console_input.set_text("p");
        wb.debug_console_submit();
        wait_for(&mut wb, "the watches", |wb| wb.debug.session.as_ref().unwrap().watch_results.iter().all(Option::is_some) && wb.debug.console.iter().any(|l| l.category == "result"));
        let s = wb.debug.session.as_ref().unwrap();
        assert_eq!(s.watch_results[0].as_ref().unwrap().as_ref().unwrap().value, "2");
        assert!(s.watch_results[1].as_ref().unwrap().is_err());
        let result = wb.debug.console.iter().find(|l| l.category == "result").unwrap();
        assert_eq!((result.text.as_str(), result.reference), ("{x:3, y:4}", 77));

        // Hovering a word evaluates it.
        let doc = wb.groups[wb.active_group].tabs[wb.groups[wb.active_group].active].doc;
        wb.hover_probe = Some(super::super::intel::HoverProbe { group: wb.active_group, doc, pos: Pos::new(6, 1), since: Instant::now() - Duration::from_secs(5), fired: false });
        wb.lsp_tick();
        wait_for(&mut wb, "the hover", |wb| wb.hover.as_ref().is_some_and(|h| h.debug.is_some()));
        assert_eq!(wb.hover.as_ref().unwrap().debug.as_deref(), Some("\"hovered\""));

        // Step Over: a new stop on the next line, with p still expanded (and fetched again).
        wb.debug_step("next");
        assert_eq!(wb.debug.session.as_ref().unwrap().state, State::Running);
        wait_for(&mut wb, "the step", |wb| wb.debug.session.as_ref().is_some_and(|s| s.state == State::Stopped && s.children.contains_key("scope/0/p")));
        assert_eq!(wb.stack_frame_mark(Some(&file)), Some((7, true)));
        assert_eq!(wb.debug.session.as_ref().unwrap().watch_results.len(), 2);
        // Another frame of the stack.
        wb.focus_frame(1);
        assert_eq!(wb.stack_frame_mark(Some(&file)), Some((15, false)));

        // Continue twice: the program ends and so does the session.
        wb.debug_continue();
        wait_for(&mut wb, "the second breakpoint hit", |wb| wb.debug.session.as_ref().is_some_and(|s| s.state == State::Stopped && !s.frames.is_empty()));
        wb.debug_continue();
        wait_for(&mut wb, "the end", |wb| wb.debug.session.is_none());
        let console: String = wb.debug.console.iter().map(|l| l.text.as_str()).collect();
        assert!(console.contains("hello from the program"), "{console}");
        assert_eq!(wb.breakpoint_marks(Some(&file)), [(6, BpLook::Normal), (19, BpLook::Normal)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_expressions_include_the_member_chain() {
        assert_eq!(hover_expression("    self.width * self.height", 9).as_deref(), Some("self.width"));
        assert_eq!(hover_expression("    self.width * self.height", 5).as_deref(), Some("self"));
        assert_eq!(hover_expression("a.b.c + 1", 4).as_deref(), Some("a.b.c"));
        assert_eq!(hover_expression("p->next", 4).as_deref(), Some("p->next"));
        assert_eq!(hover_expression("x + 1", 4), None);
        assert_eq!(hover_expression("x + y", 2), None);
    }

    #[test]
    fn a_pre_launch_task_runs_first() {
        let launch = r#"{ "version": "0.2.0", "configurations": [
            { "type": "lldb-dap", "request": "launch", "name": "Built", "program": "p", "preLaunchTask": "build" },
            { "type": "lldb-dap", "request": "launch", "name": "Broken", "program": "p", "preLaunchTask": "fail" } ] }"#;
        let tasks = r#"{ "version": "2.0.0", "tasks": [
            { "label": "build", "type": "shell", "command": "echo building" },
            { "label": "fail", "type": "shell", "command": "exit 2" } ] }"#;
        let (mut wb, dir, file) = demo_folder("task", launch, tasks);
        wb.toggle_breakpoint_at(&file, 6);
        wb.debug.selected_config = Some("Built".into());
        wb.debug_start(false);
        assert!(wb.debug.session.is_none() && wb.debug.pre_launch.is_some(), "waits for the task");
        wait_for(&mut wb, "the stop after the task", |wb| wb.debug.session.as_ref().is_some_and(|s| s.state == State::Stopped));
        assert!(wb.running_tasks().is_empty());
        wb.debug_stop();
        wait_for(&mut wb, "the end", |wb| wb.debug.session.is_none());

        wb.debug.selected_config = Some("Broken".into());
        wb.debug_start(false);
        wait_for(&mut wb, "the failed task", |wb| wb.debug.pre_launch.is_none());
        assert!(wb.debug.session.is_none());
        assert!(wb.debug.console.iter().any(|l| l.text.contains("The preLaunchTask 'fail' failed.")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_template_is_valid_and_in_order() {
        let text = launch_template("lldb-dap", Some("dbgdemo"));
        let v: Value = serde_json::from_str(&theme::strip_jsonc(&text)).unwrap();
        assert_eq!(v["configurations"][0]["program"], "${workspaceFolder}/target/debug/dbgdemo");
        assert!(text.find("\"type\"").unwrap() < text.find("\"request\"").unwrap());
        assert!(text.contains("\n            \"name\": \"Debug dbgdemo\",\n"));
        for ty in ["node", "go"] {
            let v: Value = serde_json::from_str(&theme::strip_jsonc(&launch_template(ty, None))).unwrap();
            assert_eq!(v["configurations"][0]["type"], ty);
        }
        let mut go = json!({ "request": "launch", "mode": "auto", "program": "/p/x_test.go" });
        go_config(&mut go, None);
        let mut rel = json!({ "request": "launch", "mode": "debug", "program": "cmd/app" });
        go_config(&mut rel, Some(Path::new("/w")));
        assert_eq!(rel["program"], "/w/cmd/app");
        assert_eq!((go["mode"].as_str(), go["program"].as_str()), (Some("test"), Some("/p")));
        let py: Value = serde_json::from_str(&theme::strip_jsonc(&launch_template("debugpy", None))).unwrap();
        assert_eq!(py["configurations"][0]["type"], "debugpy");
    }

    #[test]
    fn reads_the_cargo_package_name() {
        assert_eq!(cargo_package_name("[package]\nname = \"dbgdemo\"\nversion = \"0.1.0\"\n").as_deref(), Some("dbgdemo"));
        assert_eq!(cargo_package_name("[workspace]\nmembers = []\n"), None);
    }
}
