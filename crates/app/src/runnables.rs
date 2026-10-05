//! Running and debugging what a language server describes as runnable: rust-analyzer's
//! runnables (`cargo test ... -- name --exact`), used by its Run/Debug code lenses and the
//! Testing view. Debugging goes through a `DebugPlan`: a build that prints the program, and
//! the launch configuration to debug it with.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// How to debug something: build it (a shell command whose output names the program), then
/// start `config` with the program filled in. Without a build, `config` starts as it is (the
/// adapter builds, like Delve).
pub struct DebugPlan {
    pub label: String,
    /// Where the build runs.
    pub dir: PathBuf,
    /// Run with the login shell; its stdout is handed to `program`.
    pub build: Option<String>,
    /// Finds the program in the build's output.
    pub program: fn(&str) -> Option<PathBuf>,
    /// The launch configuration; `"program"` is set from the build.
    pub config: Value,
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-./=:,+@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// A rust-analyzer runnable (`rust-analyzer.runSingle`'s argument) as a shell command line and
/// its folder: `cargo <cargoArgs> <cargoExtraArgs> -- <executableArgs>`.
pub fn command(runnable: &Value) -> Option<(String, PathBuf)> {
    let args = &runnable["args"];
    let program = args["overrideCargo"].as_str().unwrap_or("cargo").to_string();
    let mut parts = vec![program];
    parts.extend(strings(&args["cargoArgs"]).iter().chain(&strings(&args["cargoExtraArgs"])).map(|s| quote(s)));
    let exe_args = strings(&args["executableArgs"]);
    if !exe_args.is_empty() {
        parts.push("--".into());
        parts.extend(exe_args.iter().map(|s| quote(s)));
    }
    let dir = args["cwd"].as_str().or(args["workspaceRoot"].as_str()).map(PathBuf::from)?;
    Some((parts.join(" "), dir))
}

/// The runnable's environment variables.
pub fn environment(runnable: &Value) -> Vec<(String, String)> {
    runnable["args"]["environment"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default()
}

/// The build that makes a runnable's executable, for debugging: `run` builds, `test` builds
/// the tests without running them; with JSON messages to find the executable.
fn debug_build_command(runnable: &Value) -> Option<String> {
    let mut args = strings(&runnable["args"]["cargoArgs"]);
    match args.first().map(String::as_str) {
        Some("run") => args[0] = "build".into(),
        Some("test") | Some("bench") => args.push("--no-run".into()),
        None => return None,
        _ => {}
    }
    args.push("--message-format=json".into());
    let quote = |s: &String| format!("'{}'", s.replace('\'', "'\\''"));
    Some(format!("cargo {}", args.iter().map(quote).collect::<Vec<_>>().join(" ")))
}

/// The executable in cargo's JSON messages (the last artifact that has one).
fn executable_from_messages(out: &str) -> Option<PathBuf> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact")
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .last()
}

/// A plan that starts `config` as it is.
pub fn direct(label: String, dir: PathBuf, config: Value) -> DebugPlan {
    DebugPlan { label, dir, build: None, program: |_| None, config }
}

/// Debugging a runnable: cargo builds it, lldb-dap runs the executable with its arguments.
pub fn debug_plan(runnable: &Value) -> Option<DebugPlan> {
    let build = debug_build_command(runnable)?;
    let (_, dir) = command(runnable)?;
    let args = &runnable["args"];
    let label = runnable["label"].as_str().unwrap_or("Debug").to_string();
    let cwd = args["cwd"].as_str().or(args["workspaceRoot"].as_str()).map_or_else(|| dir.clone(), PathBuf::from);
    let config = json!({
        "type": "lldb-dap",
        "request": "launch",
        "name": label,
        "args": args["executableArgs"].clone(),
        "cwd": cwd,
    });
    Some(DebugPlan { label, dir, build: Some(build), program: executable_from_messages, config })
}

/// Runs `plan`'s build (in the calling thread): the program, or why there's none.
pub fn build(plan_build: &str, dir: &Path, program: fn(&str) -> Option<PathBuf>) -> Result<PathBuf, String> {
    let shell = std::env::var("SHELL").ok().filter(|s| Path::new(s).is_file()).unwrap_or_else(|| "/bin/zsh".into());
    let out = std::process::Command::new(shell).args(["-l", "-c", plan_build]).current_dir(dir).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        program(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "The build made no executable to debug.".to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(12).collect();
        Err(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runnables_become_cargo_commands() {
        let r = json!({ "label": "test tests::it_works", "kind": "cargo", "args": {
            "workspaceRoot": "/p", "cargoArgs": ["test", "--package", "demo", "--lib"],
            "executableArgs": ["tests::it_works", "--exact", "--nocapture"] } });
        let (line, dir) = command(&r).unwrap();
        assert_eq!(line, "cargo test --package demo --lib -- tests::it_works --exact --nocapture");
        assert_eq!(dir, PathBuf::from("/p"));
        assert_eq!(debug_build_command(&r).unwrap(), "cargo 'test' '--package' 'demo' '--lib' '--no-run' '--message-format=json'");
        let run = json!({ "args": { "workspaceRoot": "/p", "cargoArgs": ["run", "--bin", "demo"], "executableArgs": [] } });
        assert_eq!(command(&run).unwrap().0, "cargo run --bin demo");
        assert!(debug_build_command(&run).unwrap().starts_with("cargo 'build'"));
        let plan = debug_plan(&r).unwrap();
        assert_eq!(plan.config["args"], json!(["tests::it_works", "--exact", "--nocapture"]));
        assert_eq!(plan.config["cwd"], "/p");
    }

    #[test]
    fn finds_the_executable() {
        let out = r#"{"reason":"compiler-artifact","executable":null}
{"reason":"compiler-artifact","executable":"/p/target/debug/deps/demo-abc"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(executable_from_messages(out), Some(PathBuf::from("/p/target/debug/deps/demo-abc")));
    }
}
