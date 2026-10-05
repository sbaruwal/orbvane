//! Credential prompts for git and ssh, answered in the editor's UI.
//!
//! git and ssh run without a terminal, so when they need a username, password, passphrase or
//! a host key confirmation they run the program in `GIT_ASKPASS` / `SSH_ASKPASS` with the
//! prompt as its argument and read the answer from its output. That program is the editor's
//! own executable in a helper mode (`client`): it passes the prompt over a Unix socket to the
//! `Server` in the running editor, which shows it and sends back the answer.
//!
//! Protocol: the client writes the prompt and closes its write half; the server replies `1`
//! followed by the answer, or `0` if the prompt was cancelled.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::thread;

use crate::Waker;

/// The environment variable that puts the editor's executable in askpass mode.
pub const SOCKET_VAR: &str = "ORBVANE_ASKPASS_SOCKET";

/// Environment for git commands that may prompt (set once the server runs).
static ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

pub(crate) fn env() -> &'static [(String, String)] {
    ENV.get().map_or(&[], Vec::as_slice)
}

/// A prompt waiting for an answer.
pub struct Request {
    pub prompt: String,
    stream: UnixStream,
}

impl Request {
    /// Whether the answer should be hidden while typed (passwords, passphrases, tokens).
    pub fn is_secret(&self) -> bool {
        let p = self.prompt.to_lowercase();
        ["password", "passphrase", "token", "pin"].iter().any(|w| p.contains(w))
    }

    /// Sends the answer (None: cancelled, so git or ssh gives up).
    pub fn answer(mut self, answer: Option<&str>) {
        let reply = match answer {
            Some(a) => format!("1{a}"),
            None => "0".into(),
        };
        let _ = self.stream.write_all(reply.as_bytes());
    }
}

pub struct Server {
    socket: PathBuf,
    requests: Receiver<Request>,
}

impl Server {
    /// Listens for prompts from git and ssh started by this process, using `program` (the
    /// editor's executable) as the askpass helper. Requests arrive through `poll`, and
    /// `waker` is called for each.
    pub fn start(program: &Path, waker: Waker) -> io::Result<Server> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let socket = std::env::temp_dir().join(format!("orbvane-askpass-{}-{nanos:x}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let (tx, requests) = mpsc::channel();
        thread::Builder::new().name("askpass".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut prompt = String::new();
                if stream.read_to_string(&mut prompt).is_err() {
                    continue;
                }
                if tx.send(Request { prompt: prompt.trim_end().to_string(), stream }).is_err() {
                    break;
                }
                waker();
            }
        })?;
        let program = program.to_string_lossy().into_owned();
        let vars = vec![
            ("GIT_ASKPASS".to_string(), program.clone()),
            ("SSH_ASKPASS".to_string(), program),
            // ssh only uses SSH_ASKPASS without a terminal and a display unless forced.
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            (SOCKET_VAR.to_string(), socket.to_string_lossy().into_owned()),
        ];
        let _ = ENV.set(vars);
        Ok(Server { socket, requests })
    }

    /// The next prompt waiting for an answer.
    pub fn poll(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// Environment variables that route prompts to this server.
    pub fn env(&self) -> &'static [(String, String)] {
        env()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Asks the editor listening on `socket`. Returns None if the prompt was cancelled.
pub fn request(socket: &Path, prompt: &str) -> io::Result<Option<String>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(prompt.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply.strip_prefix('1').map(str::to_string))
}

/// The askpass helper: forwards `prompt` to the editor and prints the answer for git or ssh.
/// Returns the process exit code (non-zero when cancelled, which makes them give up).
pub fn client(socket: &Path, prompt: &str) -> i32 {
    match request(socket, prompt) {
        Ok(Some(answer)) => {
            println!("{answer}");
            0
        }
        Ok(None) => 1,
        Err(e) => {
            eprintln!("askpass: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn prompts_round_trip() {
        let server = Server::start(Path::new("/usr/bin/true"), Arc::new(|| {})).unwrap();
        let socket = PathBuf::from(&env().iter().find(|(k, _)| k == SOCKET_VAR).unwrap().1);
        let answers = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen = Vec::new();
            while seen.len() < 2 && Instant::now() < deadline {
                if let Some(r) = server.poll() {
                    seen.push((r.prompt.clone(), r.is_secret()));
                    let cancel = seen.len() == 2;
                    r.answer(if cancel { None } else { Some("alice") });
                }
                thread::sleep(Duration::from_millis(5));
            }
            seen
        });
        assert_eq!(request(&socket, "Username for 'https://example.com': ").unwrap().as_deref(), Some("alice"));
        assert_eq!(request(&socket, "Password for 'https://alice@example.com': ").unwrap(), None);
        let seen = answers.join().unwrap();
        assert_eq!(seen[0], ("Username for 'https://example.com':".to_string(), false));
        assert!(seen[1].1);
    }
}
