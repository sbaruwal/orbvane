//! Runs a real shell on a PTY.

use std::sync::Arc;
use std::time::{Duration, Instant};

use terminal::Terminal;

fn wait_for(term: &Terminal, what: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let e = term.lock();
        let rows: Vec<String> = (0..e.term.rows()).filter_map(|r| e.term.visible_row(r, 0).map(|row| row.text())).collect();
        if rows.iter().any(|r| r.contains(what)) {
            return true;
        }
        drop(e);
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn runs_commands_and_reports_size() {
    let dir = std::env::temp_dir();
    let term = Terminal::spawn(&dir, 80, 24, Arc::new(|| {})).unwrap();
    term.write(b"echo orbvane-$((6*7))\r");
    assert!(wait_for(&term, "orbvane-42"), "echo output missing");
    // The PTY size is visible to programs.
    term.write(b"stty size\r");
    assert!(wait_for(&term, "24 80"), "stty size missing");
    term.resize(100, 30);
    term.write(b"stty size\r");
    assert!(wait_for(&term, "30 100"), "resize not applied");
    term.write(b"exit 3\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !term.has_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(term.has_exited());
    assert_eq!(term.exit_code(), Some(3));
}
