//! git asks for credentials through the editor: the `orbvane` binary in askpass mode
//! forwards git's prompts to `scm::askpass::Server`, whose answers git then uses.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn git_prompts_are_answered_by_the_editor() {
    let exe = Path::new(env!("CARGO_BIN_EXE_orbvane"));
    let server = scm::askpass::Server::start(exe, Arc::new(|| {})).unwrap();
    let env = server.env();

    let mut git = Command::new("git")
        .args(["-c", "credential.helper=", "credential", "fill"])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    git.stdin.take().unwrap().write_all(b"protocol=https\nhost=example.com\n\n").unwrap();

    let mut prompts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while prompts.len() < 2 && Instant::now() < deadline {
        match server.poll() {
            Some(r) => {
                let answer = if r.is_secret() { "s3cret pass" } else { "alice" };
                prompts.push(r.prompt.clone());
                r.answer(Some(answer));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    if prompts.len() < 2 {
        // Don't hang on a git still waiting for its helper: fail with what we got.
        let _ = git.kill();
    }
    let out = git.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert_eq!(prompts, ["Username for 'https://example.com':", "Password for 'https://alice@example.com':"]);
    assert!(out.contains("username=alice"), "{out}");
    assert!(out.contains("password=s3cret pass"), "{out}");
}
