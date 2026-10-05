//! Tasks: `.orbvane/tasks.json` ("shell", "process" and rust-analyzer's "cargo"
//! tasks, with the `osx` overrides and `${...}` variables) plus detected tasks,
//! task providers: Cargo ("rust: cargo build"...), Go ("go: build package"...) and npm
//! scripts ("npm: build", run with the project's package manager). A task runs in a terminal of its own through the
//! login shell (`terminal_view.rs`). A debug configuration's `preLaunchTask` runs first and
//! the session starts when it succeeds.

use std::path::PathBuf;

use serde_json::Value;

use super::debug::substitute;
use super::{Focus, Workbench};
use crate::palette::{Action, Item, Palette, Picker};

/// A task ready to run.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Task {
    pub label: String,
    /// The command line, for the login shell.
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// "build", "test" or "".
    pub group: String,
    pub is_default: bool,
    /// Where it comes from ("rust", "go", "npm" for detected tasks), shown in pickers.
    pub source: &'static str,
}

/// Quotes `arg` for the shell when it needs it.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_-./=:,+@%".contains(c));
    if plain { arg.to_string() } else { format!("'{}'", arg.replace('\'', "'\\''")) }
}

/// An argument: a string, or `{ "value": ..., "quoting": ... }`.
fn arg_text(v: &Value) -> Option<String> {
    v.as_str().or_else(|| v["value"].as_str()).map(String::from)
}

/// One `tasks.json` entry as a task (None for task types we can't run).
fn parse_task(raw: &Value) -> Option<Task> {
    // The macOS overrides replace fields of the task.
    let mut t = raw.clone();
    if let (Some(obj), Some(osx)) = (t.as_object_mut(), raw["osx"].as_object()) {
        for (k, v) in osx {
            obj.insert(k.clone(), v.clone());
        }
    }
    let ty = t["type"].as_str().unwrap_or("process");
    let command = t["command"].as_str().map(String::from).or_else(|| t["command"]["value"].as_str().map(String::from))?;
    let args: Vec<String> = t["args"].as_array().map(|a| a.iter().filter_map(arg_text).collect()).unwrap_or_default();
    let (line, default_label) = match ty {
        // A shell command line runs as written; its args are quoted onto it.
        "shell" => (std::iter::once(command.clone()).chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" "), command.clone()),
        "process" => (format!("exec {}", std::iter::once(&command).chain(&args).map(|a| quote(a)).collect::<Vec<_>>().join(" ")), command.clone()),
        "cargo" => {
            let line = ["cargo".to_string(), quote(&command)].into_iter().chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ");
            (line, format!("cargo {command}"))
        }
        _ => return None,
    };
    let (group, is_default) = match &t["group"] {
        Value::String(g) => (g.clone(), false),
        Value::Object(o) => (o.get("kind").and_then(Value::as_str).unwrap_or_default().to_string(), o.get("isDefault").and_then(Value::as_bool).unwrap_or(false)),
        _ => (String::new(), false),
    };
    let env = t["options"]["env"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default();
    Some(Task {
        label: t["label"].as_str().map(String::from).unwrap_or(default_label),
        command: line,
        cwd: t["options"]["cwd"].as_str().map(PathBuf::from),
        env,
        group,
        is_default,
        source: "",
    })
}

/// rust-analyzer's Cargo tasks, for a folder with a Cargo.toml.
fn cargo_tasks() -> Vec<Task> {
    [("build", "build"), ("check", "build"), ("test", "test"), ("run", ""), ("clean", "")]
        .into_iter()
        .map(|(cmd, group)| Task {
            label: format!("rust: cargo {cmd}"),
            command: format!("cargo {cmd}"),
            cwd: None,
            env: Vec::new(),
            group: group.into(),
            is_default: false,
            source: "rust",
        })
        .collect()
}

/// Go tasks, for a folder with a go.mod: the package is the active file's folder.
fn go_tasks(package_dir: Option<PathBuf>) -> Vec<Task> {
    [("build package", "go build -v .", "build", true), ("test package", "go test -v .", "test", true), ("build workspace", "go build -v ./...", "build", false), ("test workspace", "go test -v ./...", "test", false)]
        .into_iter()
        .map(|(name, command, group, package)| Task {
            label: format!("go: {name}"),
            command: command.into(),
            cwd: if package { package_dir.clone() } else { None },
            env: Vec::new(),
            group: group.into(),
            is_default: false,
            source: "go",
        })
        .collect()
}

/// The package manager a Node project uses, from its lock file.
fn package_manager(folder: &std::path::Path) -> &'static str {
    [("bun.lockb", "bun"), ("bun.lock", "bun"), ("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn")].into_iter().find(|(f, _)| folder.join(f).is_file()).map_or("npm", |(_, pm)| pm)
}

/// The npm tasks: `npm: install` and one per package.json script (`build` and `test`
/// in their groups).
fn npm_tasks(folder: &std::path::Path, package_json: &str) -> Vec<Task> {
    let Ok(v) = serde_json::from_str::<Value>(&theme::strip_jsonc(package_json)) else { return Vec::new() };
    let pm = package_manager(folder);
    let task = |label: String, command: String, group: &str| Task { label, command, cwd: None, env: Vec::new(), group: group.into(), is_default: false, source: "npm" };
    let mut out = vec![task("npm: install".into(), format!("{pm} install"), "")];
    for name in v["scripts"].as_object().into_iter().flat_map(|o| o.keys()) {
        let group = match name.as_str() {
            "build" => "build",
            "test" => "test",
            _ => "",
        };
        out.push(task(format!("npm: {name}"), format!("{pm} run {}", quote(name)), group));
    }
    out
}

const TASKS_TEMPLATE: &str = r#"{
    // Tasks for this folder: run them with Terminal > Run Task...
    "version": "2.0.0",
    "tasks": [
        {
            "label": "echo",
            "type": "shell",
            "command": "echo Hello"
        }
    ]
}
"#;

impl Workbench {
    fn tasks_json(&self) -> Option<PathBuf> {
        Some(self.folder()?.join(".orbvane").join("tasks.json"))
    }

    /// The folder's tasks: `tasks.json`'s, then the detected ones it doesn't redefine.
    pub(super) fn tasks(&self) -> Result<Vec<Task>, String> {
        let Some(folder) = self.folder() else { return Ok(Vec::new()) };
        let mut tasks = Vec::new();
        if let Some(text) = self.tasks_json().and_then(|p| std::fs::read_to_string(p).ok()) {
            let v: Value = serde_json::from_str(&theme::strip_jsonc(&text)).map_err(|e| format!("tasks.json: {e}"))?;
            let vars = |n: &str| self.launch_variable(n);
            for raw in v["tasks"].as_array().into_iter().flatten() {
                if let Some(t) = parse_task(&substitute(raw, &vars)) {
                    tasks.push(t);
                }
            }
        }
        // Detected tasks, unless tasks.json redefines them.
        let mut detected = Vec::new();
        if folder.join("Cargo.toml").is_file() {
            detected.extend(cargo_tasks());
        }
        if folder.join("go.mod").is_file() {
            let package = self.launch_variable("fileDirname").map(PathBuf::from).filter(|d| d.starts_with(&folder));
            detected.extend(go_tasks(package));
        }
        if let Ok(text) = std::fs::read_to_string(folder.join("package.json")) {
            detected.extend(npm_tasks(&folder, &text));
        }
        let defined: Vec<String> = tasks.iter().map(|t| t.label.clone()).collect();
        tasks.extend(detected.into_iter().filter(|t| !defined.contains(&t.label)));
        Ok(tasks)
    }

    /// Runs the task labeled `label`. Returns false (after saying why) when it can't.
    pub(super) fn run_task(&mut self, label: &str) -> bool {
        let tasks = match self.tasks() {
            Ok(t) => t,
            Err(e) => {
                self.task_error(&e);
                return false;
            }
        };
        let Some(task) = tasks.into_iter().find(|t| t.label == label) else {
            self.task_error(&format!("Could not find the task '{label}'."));
            return false;
        };
        let Some(folder) = self.folder() else { return false };
        let cwd = task.cwd.clone().filter(|p| p.is_dir()).unwrap_or(folder);
        match self.run_task_terminal(&task.label, &task.command, &cwd, &task.env) {
            Ok(()) => true,
            Err(e) => {
                self.task_error(&e);
                false
            }
        }
    }

    fn task_error(&mut self, msg: &str) {
        if cfg!(test) {
            return self.set_status_message(msg);
        }
        self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Tasks").set_description(msg).show();
    }

    fn task_picker(&mut self, tasks: Vec<Task>, placeholder: &str) {
        let choices = tasks
            .into_iter()
            .map(|t| Item {
                label: t.label.clone(),
                detail: if t.source.is_empty() { String::new() } else { t.source.to_string() },
                matches: Vec::new(),
                shortcut: None,
                action: Action::Task(t.label),
                group: None,
                kind: None,
            })
            .collect();
        self.palette = Some(Palette::with_picker(Picker { placeholder: placeholder.into(), choices }));
    }

    /// Tasks: Run Task.
    pub(super) fn run_task_prompt(&mut self) {
        match self.tasks() {
            Ok(t) if t.is_empty() => self.task_error("No tasks found. Configure tasks in .orbvane/tasks.json (Tasks: Configure Task)."),
            Ok(t) => self.task_picker(t, "Select the task to run"),
            Err(e) => self.task_error(&e),
        }
    }

    /// Tasks: Run Build Task (⇧⌘B): the default build task, or a choice of build tasks.
    pub(super) fn run_build_task(&mut self) {
        let tasks = match self.tasks() {
            Ok(t) => t,
            Err(e) => return self.task_error(&e),
        };
        if let Some(t) = tasks.iter().find(|t| t.group == "build" && t.is_default) {
            let label = t.label.clone();
            self.run_task(&label);
            return;
        }
        let build: Vec<Task> = tasks.into_iter().filter(|t| t.group == "build").collect();
        if build.is_empty() {
            return self.task_error("No build task to run found. Configure Build Task...");
        }
        self.task_picker(build, "Select the build task to run");
    }

    /// Tasks: Terminate Task.
    pub(super) fn terminate_task(&mut self) {
        let running = self.running_tasks();
        match running.len() {
            0 => self.set_status_message("No task is currently running."),
            1 => self.terminate_task_terminal(&running[0]),
            _ => {
                let choices = running
                    .into_iter()
                    .map(|l| Item { label: l.clone(), detail: String::new(), matches: Vec::new(), shortcut: None, action: Action::TerminateTask(l), group: None, kind: None })
                    .collect();
                self.palette = Some(Palette::with_picker(Picker { placeholder: "Select a task to terminate".into(), choices }));
            }
        }
    }

    /// Tasks: Configure Task: opens `tasks.json`, creating it first.
    pub(super) fn configure_tasks(&mut self) {
        let Some(path) = self.tasks_json() else { return self.task_error("Open a folder to configure tasks.") };
        if !path.exists() {
            if let Err(e) = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, TASKS_TEMPLATE)) {
                return self.task_error(&format!("Unable to create 'tasks.json' ({e})."));
            }
        }
        self.open_file(&path);
        self.focus = Focus::Editor;
    }

    /// Reports finished tasks: a debug session waiting on its preLaunchTask starts (or,
    /// when the task failed, asks what to do). Called every frame.
    pub(super) fn tasks_tick(&mut self) {
        for (label, code) in self.finished_tasks() {
            if self.assistant_task_done(&label, code) {
                continue;
            }
            match self.installing.remove(&label) {
                Some(command) => self.server_install_done(command, code),
                None => self.debug_pre_launch_done(&label, code),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_go_and_npm_tasks() {
        let go = go_tasks(Some(PathBuf::from("/p/pkg")));
        assert_eq!((go[0].label.as_str(), go[0].command.as_str(), go[0].cwd.as_deref()), ("go: build package", "go build -v .", Some(std::path::Path::new("/p/pkg"))));
        assert_eq!((go[3].label.as_str(), go[3].cwd.as_deref()), ("go: test workspace", None));
        let npm = npm_tasks(std::path::Path::new("/nowhere"), r#"{ "scripts": { "build": "tsc", "test": "jest", "start:dev": "node ." } }"#);
        let got: Vec<(&str, &str, &str)> = npm.iter().map(|t| (t.label.as_str(), t.command.as_str(), t.group.as_str())).collect();
        assert_eq!(got, [("npm: install", "npm install", ""), ("npm: build", "npm run build", "build"), ("npm: test", "npm run test", "test"), ("npm: start:dev", "npm run start:dev", "")]);
    }

    #[test]
    fn parses_tasks() {
        let t = parse_task(&json!({ "label": "build", "type": "shell", "command": "make -j4", "args": ["all", "a b"], "group": { "kind": "build", "isDefault": true } })).unwrap();
        assert_eq!((t.command.as_str(), t.group.as_str(), t.is_default), ("make -j4 all 'a b'", "build", true));
        let p = parse_task(&json!({ "type": "process", "command": "/bin/echo", "args": [{ "value": "it's", "quoting": "strong" }] })).unwrap();
        assert_eq!((p.label.as_str(), p.command.as_str()), ("/bin/echo", "exec /bin/echo 'it'\\''s'"));
        let c = parse_task(&json!({ "type": "cargo", "command": "build", "args": ["--release"], "group": "build", "osx": { "args": ["-v"] } })).unwrap();
        assert_eq!((c.label.as_str(), c.command.as_str()), ("cargo build", "cargo build -v"));
        assert!(parse_task(&json!({ "type": "npm", "script": "watch" })).is_none());
    }
}
