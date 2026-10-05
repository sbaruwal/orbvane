//! When language servers run: they start for the files on screen, stop when idle
//! (`languageServers.stopWhenIdle`) and can be restarted or stopped by hand. A server that
//! isn't installed is reported once, with a button that installs it in a terminal; when that
//! succeeds the server starts without restarting the editor.

use std::collections::HashSet;
use std::time::Instant;

use language::Lang;

use super::notifications::Severity;
use super::Workbench;
use crate::config;
use crate::servers::{MissingServer, ServerKey, Servers};

const INSTALL: &str = "Install";
const COPY: &str = "Copy Install Command";
const TRY_AGAIN: &str = "Try Again";

impl Workbench {
    /// The documents on screen (each group's active tab).
    pub(super) fn shown_docs(&self) -> HashSet<usize> {
        self.groups.iter().filter_map(|g| g.tabs.get(g.active)).map(|ed| ed.doc).collect()
    }

    /// Stops the servers that have been idle long enough. Called every frame, after the
    /// servers of the files on screen were marked as used.
    pub(super) fn idle_servers_tick(&mut self) {
        let c = config::get();
        if self.lsp.idle_deadline(c.idle_stop, c.idle_after).is_some_and(|t| t <= Instant::now()) {
            self.lsp.stop_idle(c.idle_stop, c.idle_after);
        }
    }

    pub(super) fn idle_servers_deadline(&self) -> Option<Instant> {
        let c = config::get();
        self.lsp.idle_deadline(c.idle_stop, c.idle_after)
    }

    /// The server for the active editor's file, whether or not it runs.
    fn active_server(&self) -> Option<(ServerKey, Lang)> {
        let ed = self.active_editor()?;
        let doc = self.docs[ed.doc].as_ref()?;
        let path = doc.buffer.path()?;
        Some((Servers::key_of(doc.lang, &self.lsp_root(path, doc.lang))?, doc.lang))
    }

    /// Developer: Restart Language Server: the active file's server, else all of them.
    pub(super) fn restart_language_server(&mut self) {
        let keys = match self.active_server() {
            Some((key, _)) => vec![key],
            None => self.lsp.running(),
        };
        if keys.is_empty() {
            return self.set_status_message("No language server is running.");
        }
        self.lsp.restart(&keys);
        let names: Vec<&str> = keys.iter().map(|k| k.0.trim_start_matches("builtin:")).collect();
        self.set_status_message(&format!("Restarting {}", names.join(", ")));
    }

    /// Developer: Stop Language Servers: frees their memory until they're restarted.
    pub(super) fn stop_language_servers(&mut self) {
        match self.lsp.stop_all() {
            0 => self.set_status_message("No language server is running."),
            n => {
                let what = if n == 1 { "1 language server".to_string() } else { format!("{n} language servers") };
                self.set_status_message(&format!("Stopped {what}. Restart Language Server starts them again."));
            }
        }
    }

    // ------------------------------------------------------------------ missing servers

    /// Tells about language servers that aren't installed, once per server a session.
    pub(super) fn missing_servers_tick(&mut self) {
        for m in self.lsp.take_missing() {
            if self.missing_servers.insert(m.command, m.clone()).is_none() {
                self.show_missing_server(&m);
            }
        }
    }

    fn show_missing_server(&mut self, m: &MissingServer) {
        let mut message = format!(
            "{} needs {}, which isn't installed, for Go to Definition, references, completion and hovers.",
            m.language, m.command
        );
        let mut actions = Vec::new();
        if let Some(install) = m.install {
            message.push_str(&format!(" Install installs it with \"{install}\" in a terminal."));
            actions = vec![INSTALL.to_string(), COPY.to_string()];
        }
        self.notify_with(Severity::Warning, &message, actions, Workbench::missing_server_clicked, m.command);
    }

    fn missing_server_clicked(&mut self, action: Option<&str>, command: &str) {
        let Some(m) = self.missing_servers.values().find(|m| m.command == command).cloned() else { return };
        let Some(install) = m.install else { return };
        match action {
            Some(INSTALL) => self.install_server(&m, install),
            Some(COPY) => {
                if let Some(cb) = &mut self.clipboard {
                    let _ = cb.set_text(install.to_string());
                }
                self.set_status_message(&format!("Copied: {install}"));
            }
            _ => {}
        }
    }

    /// Runs `install` in a terminal; `server_install_done` follows.
    fn install_server(&mut self, m: &MissingServer, install: &str) {
        let label = format!("Install {}", m.command);
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/".into());
        let cwd = self.folder().filter(|f| f.is_dir()).unwrap_or(home);
        match self.run_task_terminal(&label, install, &cwd, &[]) {
            Ok(()) => {
                self.installing.insert(label, m.command);
            }
            Err(e) => self.notify(Severity::Error, &e, "", Vec::new(), None),
        }
    }

    /// The install task for server `command` ended with `code`.
    pub(super) fn server_install_done(&mut self, command: &'static str, code: Option<i32>) {
        if code != Some(0) {
            let message = format!("Installing {command} failed. The terminal shows what went wrong.");
            return self.notify(Severity::Error, &message, "", Vec::new(), None);
        }
        if crate::servers::find_binary(command).is_none() {
            // Some installers finish in a window of their own (the macOS developer tools).
            let message = format!("{command} still can't be found. If an installer opened, finish it, then try again.");
            return self.notify_with(Severity::Warning, &message, vec![TRY_AGAIN.to_string()], Workbench::server_try_again, command);
        }
        self.server_installed(command);
        self.notify(Severity::Info, &format!("{command} is installed and starting."), "", Vec::new(), None);
    }

    fn server_try_again(&mut self, action: Option<&str>, command: &str) {
        if action != Some(TRY_AGAIN) {
            return;
        }
        let Some(command) = self.missing_servers.keys().copied().find(|c| *c == command) else { return };
        // Still missing: the server is reported again, with its buttons.
        self.server_installed(command);
    }

    /// Server `command` may be installed now: it starts again for the files on screen.
    fn server_installed(&mut self, command: &'static str) {
        self.missing_servers.remove(command);
        self.lsp.forget_missing(command);
    }

    /// A language feature was asked for in a `lang` file that has no server running: says why.
    pub(super) fn explain_no_server(&mut self, feature: &str, lang: Lang) {
        if let Some(m) = self.missing_servers.values().find(|m| m.language == lang.name()).cloned() {
            return self.show_missing_server(&m);
        }
        let key = self.active_editor().and_then(|ed| self.docs[ed.doc].as_ref()).and_then(|d| {
            let path = d.buffer.path()?;
            Servers::key_of(lang, &self.lsp_root(path, lang))
        });
        let message = match key {
            Some(k) if self.lsp.is_held(&k) => format!("{feature} needs {}, which was stopped. Run Developer: Restart Language Server to start it.", k.0),
            Some(k) if self.lsp.ready(&k).is_none() => format!("{} stopped working. Run Developer: Restart Language Server to start it again.", k.0),
            Some(k) => format!("{feature} needs {}, which is still starting.", k.0),
            None => format!("{feature} needs a language server, and none is set up for {}.", lang.name()),
        };
        self.set_status_message(&message);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;

    /// A workbench on a scratch folder with `files` open (the last one shown).
    fn workbench_with(name: &str, files: &[(&str, &str)]) -> (Workbench, PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbvane-servers-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let paths: Vec<PathBuf> = files.iter().map(|(f, _)| dir.join(f)).collect();
        (Workbench::new(Some(dir.clone()), &paths, std::sync::Arc::new(|| {})), dir)
    }

    /// Ticks until `done` holds (the built-in servers answer on threads of their own).
    fn tick_until(wb: &mut Workbench, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            wb.lsp_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn show(wb: &mut Workbench, path: &std::path::Path) {
        wb.open_file(path);
        wb.lsp_tick();
    }

    #[test]
    fn servers_start_for_the_files_on_screen_and_stop_when_idle() {
        let (mut wb, dir) = workbench_with("idle", &[("a.json", "{\"a\": 1}\n"), ("notes.txt", "hello\n")]);
        let (json, txt) = (dir.join("a.json"), dir.join("notes.txt"));
        // Only the file on screen goes to a server: the JSON file behind it waits.
        wb.lsp_tick();
        assert!(!wb.lsp.is_open(&json));
        assert!(wb.lsp.running().is_empty());
        show(&mut wb, &json);
        tick_until(&mut wb, "the JSON server", |wb| wb.lsp.is_running(&json));
        let key = wb.lsp.running();
        assert_eq!(key.len(), 1);
        assert_eq!(key[0].0, "builtin:json");

        // Shown, it never counts as idle.
        let mut c = config::get();
        c.idle_after = Duration::ZERO;
        config::set(c);
        for _ in 0..20 {
            wb.lsp_tick();
        }
        assert_eq!(wb.lsp.running().len(), 1);

        // Hidden, it stops (once it answered what it was asked), and comes back when shown.
        show(&mut wb, &txt);
        tick_until(&mut wb, "the idle server to stop", |wb| wb.lsp.running().is_empty());
        assert!(!wb.lsp.is_open(&json));
        show(&mut wb, &json);
        tick_until(&mut wb, "the server to start again", |wb| wb.lsp.is_running(&json));

        // Off: nothing stops.
        c.idle_stop = crate::servers::IdleStop::Off;
        config::set(c);
        show(&mut wb, &txt);
        for _ in 0..20 {
            wb.lsp_tick();
        }
        assert_eq!(wb.lsp.running().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_outside_the_folder_join_its_server() {
        let (mut wb, dir) = workbench_with("outside", &[("a.json", "{\"a\": 1}\n")]);
        let json = dir.join("a.json");
        tick_until(&mut wb, "the JSON server", |wb| wb.lsp.is_running(&json));
        // A library's file elsewhere (where Go to Definition leads) goes to the same server,
        // not one of its own rooted next to it.
        let lib = std::env::temp_dir().join(format!("orbvane-servers-{}-library", std::process::id()));
        std::fs::create_dir_all(lib.join("src")).unwrap();
        let other = lib.join("src/b.json");
        std::fs::write(&other, "{\"b\": 2}\n").unwrap();
        for path in [&other, &json, &other] {
            show(&mut wb, path);
            tick_until(&mut wb, "the file to open", |wb| wb.lsp.is_running(path));
        }
        let running = wb.lsp.running();
        assert_eq!(running.len(), 1, "{running:?}");
        assert_eq!(running[0].1, dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn heavy_servers_keep_running_unless_asked() {
        let lang = Lang::all().find(|l| l.def().server.as_ref().is_some_and(|s| s.command == "rust-analyzer")).unwrap();
        assert!(lang.def().server.as_ref().unwrap().heavy);
        assert!(!Lang::all().filter_map(|l| l.def().server.as_ref()).any(|s| s.command == "builtin:json" && s.heavy));
        assert_eq!(crate::servers::IdleStop::parse("all"), crate::servers::IdleStop::All);
        assert_eq!(crate::servers::IdleStop::parse("light"), crate::servers::IdleStop::Light);
        assert_eq!(crate::servers::IdleStop::parse("off"), crate::servers::IdleStop::Off);
    }

    #[test]
    fn stopping_and_restarting_servers() {
        let (mut wb, dir) = workbench_with("restart", &[("a.json", "{\"a\": 1}\n")]);
        let json = dir.join("a.json");
        tick_until(&mut wb, "the JSON server", |wb| wb.lsp.is_running(&json));
        wb.run(crate::commands::Command::StopLanguageServers);
        assert!(wb.lsp.running().is_empty());
        // Stopped servers stay stopped while their file is on screen, and say why.
        for _ in 0..20 {
            wb.lsp_tick();
        }
        assert!(wb.lsp.running().is_empty());
        let lang = wb.docs.iter().flatten().next().unwrap().lang;
        wb.explain_no_server("Go to Definition", lang);
        let status = wb.status_message.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
        assert!(status.contains("was stopped") && status.contains("Restart Language Server"), "{status}");

        wb.run(crate::commands::Command::RestartLanguageServer);
        tick_until(&mut wb, "the restarted server", |wb| wb.lsp.is_running(&json));
        // Restarting a running one replaces it.
        wb.run(crate::commands::Command::RestartLanguageServer);
        assert!(!wb.lsp.is_open(&json));
        tick_until(&mut wb, "the server to start again", |wb| wb.lsp.is_running(&json));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installing_a_missing_server() {
        let (mut wb, dir) = workbench_with("install", &[("notes.txt", "hello\n")]);
        let command = "orbvane-test-missing-server";
        let m = MissingServer { language: "Test", command, install: Some("true") };
        wb.missing_servers.insert(command, m.clone());
        wb.show_missing_server(&m);
        let toast = wb.toast_list().into_iter().find(|(_, msg, _)| msg.starts_with("Test needs")).unwrap();
        assert_eq!(toast.2, [INSTALL, COPY]);
        assert!(toast.1.contains("\"true\" in a terminal"), "{}", toast.1);

        // The install runs as a task; when it ends the result is reported.
        wb.close_toast(toast.0, Some(0));
        assert_eq!(wb.installing.get(&format!("Install {command}")), Some(&command));
        let start = Instant::now();
        while !wb.installing.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(10), "the install task didn't end");
            wb.tasks_tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        // It succeeded, but nothing called that exists: the user can try again.
        let toasts = wb.toast_list();
        let again = toasts.iter().find(|(_, msg, _)| msg.contains("still can't be found")).expect("no try again");
        assert_eq!(again.2, [TRY_AGAIN]);

        wb.server_install_done(command, Some(1));
        assert!(wb.toast_list().iter().any(|(_, msg, _)| msg.starts_with(&format!("Installing {command} failed"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_server_found_after_installing_starts() {
        let (mut wb, dir) = workbench_with("installed", &[("notes.txt", "hello\n")]);
        // `sh` exists, so the "installed" server is found and forgotten as missing.
        let m = MissingServer { language: "Test", command: "sh", install: Some("true") };
        wb.missing_servers.insert("sh", m);
        wb.server_install_done("sh", Some(0));
        assert!(wb.missing_servers.is_empty());
        assert!(wb.toast_list().iter().any(|(_, msg, _)| msg == "sh is installed and starting."));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
