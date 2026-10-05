//! The coding agents the Assistant knows how to start (`assistant.agent`), and a check of
//! whether each one's command line tool is installed and signed in, run through the login shell
//! on a thread of its own (`probe`).
//!
//! Each agent's tool speaks its own protocol; Orbvane talks to it directly, so nothing else
//! needs installing. Any other agent that speaks the Agent Client Protocol can be started with
//! a command of the user's (`assistant.agent.command`).

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub struct Agent {
    /// The `assistant.agent` value.
    pub id: &'static str,
    pub name: &'static str,
    /// The command line tool it runs.
    pub program: &'static str,
    /// One line about it, for the setup screen.
    pub about: &'static str,
    /// A shell command that installs the tool (run in a terminal).
    pub install: &'static str,
    /// A shell command that signs in (run in a terminal, since it may open a browser).
    pub sign_in: &'static str,
    /// A shell command that reports whether it's signed in, and what its output contains if so.
    status: &'static str,
    signed_in_marker: &'static str,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude-code",
        name: "Claude Code",
        program: "claude",
        about: "Anthropic's coding agent. Uses your Claude subscription or API key.",
        install: "if command -v brew >/dev/null; then brew install --cask claude-code; else curl -fsSL https://claude.ai/install.sh | bash; fi",
        sign_in: "claude auth login",
        status: "claude auth status",
        signed_in_marker: "\"loggedIn\": true",
    },
    Agent {
        id: "codex",
        name: "Codex",
        program: "codex",
        about: "OpenAI's coding agent. Uses your ChatGPT plan or API key.",
        install: "if command -v brew >/dev/null; then brew install --cask codex; else npm install -g @openai/codex; fi",
        sign_in: "codex login",
        status: "codex login status",
        signed_in_marker: "Logged in",
    },
];

pub fn find(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

/// The agent the Assistant uses, from `assistant.agent` and `assistant.agent.command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    None,
    Builtin(&'static Agent),
    /// An Agent Client Protocol agent started with this command line.
    Custom(String),
}

impl Choice {
    /// Identifies the agent (a different one means restarting).
    pub fn key(&self) -> String {
        match self {
            Choice::None => String::new(),
            Choice::Builtin(a) => a.id.to_string(),
            Choice::Custom(command) => format!("custom:{command}"),
        }
    }

    /// Its name for the agent menu ("Codex", or a custom command's program).
    pub fn label(&self) -> String {
        match self {
            Choice::None => "Choose Agent".into(),
            Choice::Builtin(a) => a.name.into(),
            Choice::Custom(command) => {
                let program = command.split_whitespace().next().unwrap_or("");
                std::path::Path::new(program).file_name().map_or_else(|| program.to_string(), |n| n.to_string_lossy().into_owned())
            }
        }
    }
}

impl PartialEq for Agent {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Agent {}
impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id)
    }
}

/// `agent` is the setting's value, `command` the custom command. A custom command that's
/// just one of our agents' tools (`codex`) means that agent: those tools don't speak the Agent
/// Client Protocol themselves.
pub fn choice(agent: &str, command: &str) -> Choice {
    let command = command.trim();
    match agent {
        "custom" if !command.is_empty() => Choice::Custom(command.to_string()),
        "custom" | "none" | "" => match AGENTS.iter().find(|a| a.program == command) {
            Some(a) => Choice::Builtin(a),
            None if command.is_empty() => Choice::None,
            None => Choice::Custom(command.to_string()),
        },
        id => find(id).map_or(Choice::None, Choice::Builtin),
    }
}

/// What a check found about an agent's tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// Where the tool is (None: not installed).
    pub path: Option<String>,
    /// Its `--version`'s first line.
    pub version: String,
    pub signed_in: bool,
}

/// The script that checks `a`: prints `path:`, `version:` and `signed:` lines. The status
/// command's output stays in the script (it can hold the account's email).
fn script(a: &Agent) -> String {
    let p = a.program;
    format!(
        "p=$(command -v {p}) || exit 0\n\
         echo \"path:$p\"\n\
         echo \"version:$({p} --version 2>/dev/null | head -n1)\"\n\
         case \"$({status} 2>&1)\" in *'{marker}'*) echo signed:yes;; *) echo signed:no;; esac\n",
        status = a.status,
        marker = a.signed_in_marker.replace('\'', "'\\''"),
    )
}

fn parse(out: &str) -> Probe {
    let mut probe = Probe::default();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("path:") {
            probe.path = Some(p.trim().to_string()).filter(|p| !p.is_empty());
        } else if let Some(v) = line.strip_prefix("version:") {
            probe.version = v.trim().to_string();
        } else if line.trim() == "signed:yes" {
            probe.signed_in = true;
        }
    }
    probe
}

/// Checks `a` on a thread: its result goes to `tx` as (id, probe), then `wake` runs.
pub fn probe(a: &'static Agent, tx: Sender<(&'static str, Probe)>, wake: impl Fn() + Send + 'static) {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let script = script(a);
    let _ = std::thread::Builder::new().name(format!("probe {}", a.id)).spawn(move || {
        let found = run(&shell, &script, Duration::from_secs(20)).map(|out| parse(&out)).unwrap_or_default();
        let _ = tx.send((a.id, found));
        wake();
    });
}

/// Runs `script` with the login shell (so PATH matches a terminal's); its output, or None
/// when it fails or takes longer than `timeout`.
fn run(shell: &str, script: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new(shell).args(["-l", "-c", script]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_the_agent() {
        let codex = find("codex").unwrap();
        assert_eq!(choice("codex", ""), Choice::Builtin(codex));
        assert_eq!(choice("none", ""), Choice::None);
        assert_eq!(choice("", "  "), Choice::None);
        assert_eq!(choice("custom", "my-agent --acp"), Choice::Custom("my-agent --acp".into()));
        assert_eq!(choice("custom", "/opt/x/my-agent --acp").label(), "my-agent");
        // A command that is one of our tools means that agent.
        assert_eq!(choice("none", "codex"), Choice::Builtin(codex));
        assert_eq!(choice("custom", "claude"), Choice::Custom("claude".into()));
        assert_eq!(choice("none", "claude"), Choice::Builtin(find("claude-code").unwrap()));
        assert_eq!(choice("unknown", "x"), Choice::None);
    }

    #[test]
    fn reads_a_probe() {
        let p = parse("path:/opt/bin/codex\nversion:codex-cli 1.2.3\nsigned:yes\n");
        assert_eq!(p, Probe { path: Some("/opt/bin/codex".into()), version: "codex-cli 1.2.3".into(), signed_in: true });
        assert_eq!(parse(""), Probe::default());
        assert!(!parse("path:/x\nversion:\nsigned:no\n").signed_in);
    }

    #[test]
    fn probes_with_the_shell() {
        // A fake agent whose tool is `sh`: installed, and "signed in" when the marker shows.
        let a: &'static Agent = Box::leak(Box::new(Agent {
            id: "fake",
            name: "Fake",
            program: "sh",
            about: "",
            install: "",
            sign_in: "",
            status: "echo 'state: Logged in as someone'",
            signed_in_marker: "Logged in",
        }));
        let out = run("/bin/sh", &script(a), Duration::from_secs(10)).unwrap();
        let p = parse(&out);
        assert!(p.path.is_some_and(|p| p.ends_with("/sh")));
        assert!(p.signed_in);
    }
}
