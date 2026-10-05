//! What the bridges (`codex`, `claude`) share: starting the agent's own tool as a child that
//! speaks JSON, one message per line, on its stdin and stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use serde_json::Value;

/// How to start the tool.
pub struct Options {
    /// The program and arguments (usually the login shell, so PATH matches a terminal's).
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Variables that belong to an agent session Orbvane may have been started from (its own
/// terminal, say): a tool started with them would act as part of that session.
fn inherited_session_var(name: &str) -> bool {
    name == "CLAUDECODE" || name.starts_with("CLAUDE_CODE_") || name == "CLAUDE_AGENT_SDK_VERSION" || name == "CODEX_THREAD_ID"
}

/// A random (version 4) UUID.
pub fn uuid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if read.is_err() {
        // Not random, but unique enough for a session id.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        b = (t ^ ((std::process::id() as u128) << 64)).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The running tool.
pub struct Lines {
    child: Child,
    stdin: ChildStdin,
}

impl Lines {
    /// Starts the tool. Each JSON line it prints goes to `events` as `Some(message)`, then
    /// `None` when its output ends; its stderr goes to `log`.
    pub fn spawn<E: From<Option<Value>> + Send + 'static>(o: &Options, events: Sender<E>, log: Sender<String>) -> std::io::Result<Self> {
        let mut command = Command::new(&o.program);
        command.args(&o.args).current_dir(&o.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(inherited_session_var) {
                command.env_remove(name);
            }
        }
        command.envs(o.env.iter().map(|(k, v)| (k, v)));
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take().expect("piped stdin");
        thread::Builder::new().name("agent tool reader".into()).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(msg) = serde_json::from_str::<Value>(line.trim()) {
                    if events.send(E::from(Some(msg))).is_err() {
                        return;
                    }
                }
            }
            let _ = events.send(E::from(None));
        })?;
        thread::Builder::new().name("agent tool stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if log.send(line).is_err() {
                    return;
                }
            }
        })?;
        Ok(Self { child, stdin })
    }

    /// Sends one message; false when the tool is gone.
    pub fn send(&mut self, msg: &Value) -> bool {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).is_ok()
    }

    /// Closes its input and ends it.
    pub fn end(mut self) {
        drop(self.stdin);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn makes_uuids() {
        let (a, b) = (super::uuid(), super::uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn knows_session_variables() {
        assert!(super::inherited_session_var("CLAUDECODE"));
        assert!(super::inherited_session_var("CLAUDE_CODE_SESSION_ID"));
        assert!(!super::inherited_session_var("ANTHROPIC_API_KEY"));
        assert!(!super::inherited_session_var("PATH"));
    }
}
