//! Integrated terminal: a shell running on a PTY, with output parsed into a `Term` on a
//! background thread. The UI locks the terminal briefly to draw it.

mod pty;
mod term;

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

pub use term::{flags, Cell, Color, CursorShape, Emulator, Row, Term};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Terminal {
    emulator: Arc<Mutex<Emulator>>,
    pty: pty::Pty,
    exited: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    /// Name of the shell, shown in the panel header.
    pub shell_name: String,
}

/// The user's login shell, falling back to zsh (the macOS default).
fn user_shell() -> String {
    std::env::var("SHELL").ok().filter(|s| Path::new(s).is_file()).unwrap_or_else(|| "/bin/zsh".into())
}

impl Terminal {
    /// Starts the user's shell as a login shell in `cwd`.
    pub fn spawn(cwd: &Path, cols: u16, rows: u16, waker: Waker) -> io::Result<Self> {
        let shell = user_shell();
        let shell_name = Path::new(&shell).file_name().map_or("sh".into(), |n| n.to_string_lossy().to_string());
        // A leading '-' in argv[0] makes it a login shell.
        let argv0 = format!("-{shell_name}");
        Self::start(&shell, &[&argv0], &shell_name, cwd, &[], cols, rows, "", waker)
    }

    /// Runs `program` with `args` (a task): `banner` is shown first, and the terminal stays
    /// open with its output after the program exits.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_program(program: &str, args: &[&str], cwd: &Path, env: &[(&str, &str)], cols: u16, rows: u16, banner: &str, waker: Waker) -> io::Result<Self> {
        let name = Path::new(program).file_name().map_or(program.to_string(), |n| n.to_string_lossy().to_string());
        let mut argv = vec![program];
        argv.extend_from_slice(args);
        Self::start(program, &argv, &name, cwd, env, cols, rows, banner, waker)
    }

    /// Runs `command` with the user's login shell (`-l -c`), so it finds what a terminal
    /// would (cargo in ~/.cargo/bin) even when the app was started from the Finder.
    pub fn spawn_shell_command(command: &str, cwd: &Path, env: &[(&str, &str)], cols: u16, rows: u16, banner: &str, waker: Waker) -> io::Result<Self> {
        let shell = user_shell();
        Self::spawn_program(&shell, &["-l", "-c", command], cwd, env, cols, rows, banner, waker)
    }

    /// `argv` includes argv[0].
    #[allow(clippy::too_many_arguments)]
    fn start(program: &str, argv: &[&str], name: &str, cwd: &Path, extra_env: &[(&str, &str)], cols: u16, rows: u16, banner: &str, waker: Waker) -> io::Result<Self> {
        let lang = std::env::var("LANG").ok().filter(|l| !l.is_empty()).unwrap_or_else(|| "en_US.UTF-8".into());
        let mut env = vec![
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "orbvane"),
            ("LANG", lang.as_str()),
        ];
        env.extend_from_slice(extra_env);
        let emulator = Arc::new(Mutex::new(Emulator::new(cols as usize, rows as usize)));
        if !banner.is_empty() {
            emulator.lock().unwrap().feed(banner.replace('\n', "\r\n").as_bytes());
        }
        let pty = pty::Pty::spawn(program, argv, cwd, &env, cols, rows)?;
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(Mutex::new(None));

        let mut reader = pty.reader()?;
        let mut replies = pty.reader()?; // same fd, used to write query replies
        let (emu, done, code, pid) = (emulator.clone(), exited.clone(), exit_code.clone(), pty.pid());
        thread::Builder::new().name("terminal reader".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let responses = {
                            let mut e = emu.lock().unwrap();
                            e.feed(&buf[..n]);
                            std::mem::take(&mut e.term.responses)
                        };
                        if !responses.is_empty() {
                            let _ = replies.write_all(&responses);
                        }
                        waker();
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // EIO means the shell exited and the PTY closed.
                    Err(_) => break,
                }
            }
            *code.lock().unwrap() = pty::wait(pid);
            done.store(true, Ordering::SeqCst);
            waker();
        })?;

        Ok(Self { emulator, pty, exited, exit_code, shell_name: name.to_string() })
    }

    /// Shows `text` in the terminal as if the program had written it (task messages).
    pub fn echo(&self, text: &str) {
        self.lock().feed(text.replace('\n', "\r\n").as_bytes());
    }

    /// Locks the terminal state for reading or drawing.
    pub fn lock(&self) -> MutexGuard<'_, Emulator> {
        self.emulator.lock().unwrap()
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = (&*self.pty.writer()).write_all(bytes);
    }

    /// Pastes text, wrapped in bracketed-paste markers if the program asked for them.
    pub fn paste(&self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.lock().term.bracketed_paste {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.as_bytes());
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut e = self.lock();
        if e.term.cols() != cols as usize || e.term.rows() != rows as usize {
            e.term.resize(cols as usize, rows as usize);
            self.pty.resize(cols, rows);
        }
    }

    /// The program running in the foreground (the shell itself when idle), for the title.
    pub fn process_name(&self) -> Option<String> {
        if self.has_exited() {
            return None;
        }
        self.pty.foreground_name()
    }

    /// The shell's working directory (split terminals start there).
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.pty.cwd()
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.has_exited() {
            self.pty.hangup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reports_process_name_and_cwd() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let t = Terminal::spawn(&dir, 80, 24, Arc::new(|| {})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // Until the child execs the shell, it still has our name.
        let mut name = None;
        while name.as_deref() != Some(t.shell_name.as_str()) && Instant::now() < deadline {
            name = t.process_name();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(name.as_deref(), Some(t.shell_name.as_str()));
        assert_eq!(t.cwd().map(|p| p.canonicalize().unwrap()), Some(dir));
    }

    #[test]
    fn runs_a_command_with_a_banner() {
        let dir = std::env::temp_dir();
        let t = Terminal::spawn_shell_command("echo out; exit 3", &dir, &[("X", "1")], 80, 24, "* Executing task\n\n", Arc::new(|| {})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !t.has_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(t.exit_code(), Some(3));
        t.echo("\ndone\n");
        let text = t.lock().term.text_between((0, 0), (10, 80));
        let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, ["* Executing task", "out", "done"]);
    }
}
