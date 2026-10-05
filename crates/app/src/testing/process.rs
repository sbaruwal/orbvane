//! A test tool running in the background (`go test -json`, pytest): its output lines arrive
//! through a channel, waking the UI, and it can be stopped.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub enum Line {
    Out(String),
    /// The process ended (its exit code, None when killed).
    Done(Option<i32>),
}

pub struct Proc {
    child: Arc<Mutex<Child>>,
    rx: Receiver<Line>,
}

impl Proc {
    /// Starts `program args` in `dir`, stdout and stderr read as lines.
    pub fn spawn(program: &Path, args: &[String], dir: &Path, env: &[(&str, &str)], waker: lsp::Waker) -> std::io::Result<Proc> {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let (tx, rx) = mpsc::channel();
        let readers = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)];
        let child = Arc::new(Mutex::new(child));
        let (eof_tx, eof_rx) = mpsc::channel::<()>();
        for stream in readers.into_iter().flatten() {
            let (tx, waker, eof_tx): (Sender<Line>, lsp::Waker, Sender<()>) = (tx.clone(), waker.clone(), eof_tx.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(Line::Out(line)).is_err() {
                        break;
                    }
                    waker();
                }
                let _ = eof_tx.send(());
            });
        }
        drop(eof_tx);
        let waiter = child.clone();
        std::thread::spawn(move || {
            // Both streams end before the exit is reported, so no output comes after it.
            while eof_rx.recv().is_ok() {}
            // Polled, so `kill` can take the lock meanwhile.
            let code = loop {
                match waiter.lock().unwrap_or_else(|e| e.into_inner()).try_wait() {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let _ = tx.send(Line::Done(code));
            waker();
        });
        Ok(Proc { child, rx })
    }

    /// What arrived since the last call.
    pub fn poll(&mut self) -> Vec<Line> {
        self.rx.try_iter().collect()
    }

    pub fn kill(&self) {
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
}
