//! Updates of the app itself (the work is `crate::updater`). With `update.mode` "default" the
//! latest release is checked for shortly after startup and every 12 hours; a newer one is
//! downloaded and verified in the background, then a notification offers to restart into it.
//! "manual" checks only when asked (Check for Updates...), "none" never. An update left waiting
//! is installed when the app quits (if that needs no password). A copy that can't replace itself
//! (outside Applications, unsigned, a bare binary) offers the release's download page instead.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use super::notifications::Severity;
use super::{Effect, Workbench};
use crate::updater::{self, Release};

/// We check for updates every 12 hours.
const INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// The first check waits for startup to settle.
const FIRST_CHECK: Duration = Duration::from_secs(10);

/// A newer release, and what's needed to install it.
struct Found {
    release: Release,
    /// Why this copy can't install it itself (then the user is sent to the download page).
    blocker: Option<String>,
    /// The code requirement the update must meet (the running app's team and bundle id).
    requirement: Option<String>,
}

enum Reply {
    /// A check's answer (None: no newer release); `manual` when the user asked.
    Checked(Result<Option<Found>, String>, bool),
    /// The update downloaded, verified and staged (where), or why not.
    Prepared(Release, Result<PathBuf, String>, bool),
    /// The update swapped in for the running app, or why not.
    Swapped(Result<(), String>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum State {
    Idle,
    Checking,
    /// Newer, but this copy can't install it: the toast offers the download page.
    Available(Release),
    Downloading(Release),
    /// Downloaded, verified and staged here, waiting for a restart.
    Ready(Release, PathBuf),
    Installing,
}

pub(super) struct Updates {
    /// Where the latest release is asked for.
    pub url: String,
    /// The running app's bundle (None for a bare binary).
    pub app: Option<PathBuf>,
    /// Where downloads go (and the update waits, when the app's folder isn't writable).
    pub dir: PathBuf,
    /// The code requirement updates must meet; None: the running app's team and bundle id
    /// (looked up when checking).
    pub requirement: Option<String>,
    pub state: State,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    next_check: Instant,
    /// This window checks on its own (one window does, with several open).
    checks: bool,
}

impl Default for Updates {
    fn default() -> Self {
        let (tx, rx) = channel();
        let app = updater::running_bundle();
        if let Some(app) = app.clone() {
            std::thread::spawn(move || updater::clean_up(&app));
        }
        Updates {
            url: updater::latest_url(),
            app,
            dir: settings::user_data_dir().join("Update"),
            requirement: None,
            state: State::Idle,
            tx,
            rx,
            next_check: Instant::now() + FIRST_CHECK,
            checks: true,
        }
    }
}

impl Workbench {
    fn update_spawn(&self, job: impl FnOnce() -> Reply + Send + 'static) {
        let (tx, waker) = (self.updates.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            waker();
        });
    }

    fn update_mode(&self) -> String {
        self.settings.string("update.mode")
    }

    pub(super) fn updates_deadline(&self) -> Option<Instant> {
        (self.updates.checks && self.update_mode() == "default").then_some(self.updates.next_check)
    }

    /// Whether this window checks for updates on its own (only one window of several does).
    pub fn set_update_checks(&mut self, on: bool) {
        self.updates.checks = on;
    }

    pub(super) fn updates_tick(&mut self) {
        if self.updates.checks && Instant::now() >= self.updates.next_check {
            self.updates.next_check = Instant::now() + INTERVAL;
            if self.update_mode() == "default" && matches!(self.updates.state, State::Idle | State::Available(_)) {
                self.check_for_updates(false);
            }
        }
        while let Ok(reply) = self.updates.rx.try_recv() {
            self.update_reply(reply);
        }
    }

    /// Check for Updates...: asks for the latest release (or says what's already under way).
    pub(super) fn check_for_updates_command(&mut self) {
        if self.update_mode() == "none" {
            self.notify(Severity::Info, "Updates are turned off. Change the update.mode setting to check for them.", "", Vec::new(), None);
            return;
        }
        match self.updates.state.clone() {
            State::Ready(..) => self.show_update_ready(),
            State::Downloading(r) => {
                self.notify(Severity::Info, &format!("Orbvane {} is downloading.", r.version), "", Vec::new(), None);
            }
            State::Checking | State::Installing => {}
            State::Idle | State::Available(_) => self.check_for_updates(true),
        }
    }

    fn check_for_updates(&mut self, manual: bool) {
        self.updates.state = State::Checking;
        let (url, app, fixed) = (self.updates.url.clone(), self.updates.app.clone(), self.updates.requirement.clone());
        self.update_spawn(move || {
            let result = updater::latest(&url).map(|release| {
                updater::is_newer(&release.version, updater::current_version()).then(|| {
                    let requirement = fixed.or_else(|| app.as_deref().and_then(updater::team_of).map(|team| updater::requirement(&team)));
                    let blocker = updater::blocker(app.as_deref(), requirement.is_some());
                    Found { release, blocker, requirement }
                })
            });
            Reply::Checked(result, manual)
        });
    }

    fn update_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Checked(Err(e), manual) => {
                self.updates.state = State::Idle;
                if manual {
                    self.notify(Severity::Error, &format!("Couldn't check for updates: {e}"), "", Vec::new(), None);
                }
            }
            Reply::Checked(Ok(None), manual) => {
                self.updates.state = State::Idle;
                if manual {
                    self.notify(Severity::Info, "There are currently no updates available.", "", Vec::new(), None);
                }
            }
            Reply::Checked(Ok(Some(found)), manual) => match (found.blocker, found.requirement, self.updates.app.clone()) {
                (None, Some(requirement), Some(app)) => self.download_update(found.release, requirement, app, manual),
                (blocker, ..) => {
                    let why = blocker.unwrap_or_else(|| "This copy of Orbvane can't install it itself.".into());
                    let message = format!("Orbvane {} is available. {why}", found.release.version);
                    self.updates.state = State::Available(found.release);
                    self.notify_with(Severity::Info, &message, vec!["Download".into(), "Later".into()], Workbench::update_available_clicked, "");
                }
            },
            Reply::Prepared(release, Ok(staged), _) => {
                self.updates.state = State::Ready(release, staged);
                self.show_update_ready();
            }
            Reply::Prepared(release, Err(e), manual) => {
                self.updates.state = State::Idle;
                if manual {
                    self.notify(Severity::Error, &format!("Couldn't download Orbvane {}: {e}", release.version), "", Vec::new(), None);
                }
            }
            Reply::Swapped(Ok(())) => {
                self.updates.state = State::Idle;
                let Some(app) = self.updates.app.clone() else { return };
                if self.quit() {
                    updater::relaunch(&app);
                    self.effects.push(Effect::Exit);
                } else {
                    self.notify(Severity::Info, "The update is installed. It starts the next time you open Orbvane.", "", Vec::new(), None);
                }
            }
            Reply::Swapped(Err(e)) => {
                self.updates.state = State::Idle;
                self.notify(Severity::Error, &e, "", Vec::new(), None);
            }
        }
    }

    /// Downloads `release`, checks it against `requirement` and stages it to replace `app`.
    fn download_update(&mut self, release: Release, requirement: String, app: PathBuf, manual: bool) {
        if manual {
            self.notify(Severity::Info, &format!("Orbvane {} is available. Downloading it now.", release.version), "", Vec::new(), None);
        }
        self.updates.state = State::Downloading(release.clone());
        let dir = self.updates.dir.clone();
        self.update_spawn(move || {
            let dmg = dir.join(format!("Orbvane-{}.dmg", release.version));
            let staged = updater::staging_path(&app, &dir);
            let result = updater::download(&release, &dmg).and_then(|()| updater::prepare(&dmg, &requirement, &staged)).map(|()| staged);
            let _ = std::fs::remove_file(&dmg);
            Reply::Prepared(release, result, manual)
        });
    }

    fn show_update_ready(&mut self) {
        let State::Ready(release, _) = &self.updates.state else { return };
        let message = format!("Orbvane {} is ready to install. Restart Orbvane to apply the update.", release.version);
        let actions = vec!["Update Now".into(), "Later".into(), "Release Notes".into()];
        self.notify_with(Severity::Info, &message, actions, Workbench::update_ready_clicked, "");
    }

    fn update_ready_clicked(&mut self, action: Option<&str>, _: &str) {
        match action {
            Some("Update Now") => self.restart_to_update(),
            Some("Release Notes") => {
                if let State::Ready(release, _) = &self.updates.state {
                    let _ = std::process::Command::new("open").arg(&release.page).spawn();
                }
                // Keep offering the restart.
                self.show_update_ready();
            }
            _ => {}
        }
    }

    fn update_available_clicked(&mut self, action: Option<&str>, _: &str) {
        if action == Some("Download") {
            if let State::Available(release) = &self.updates.state {
                let _ = std::process::Command::new("open").arg(&release.page).spawn();
            }
        }
    }

    /// Whether an update is waiting for a restart (the gear's badge).
    pub(super) fn update_ready(&self) -> bool {
        matches!(self.updates.state, State::Ready(..))
    }

    /// The gear menu's update entry: its label, and the command it runs (None: shown disabled).
    pub(super) fn update_menu_entry(&self) -> (String, Option<crate::commands::Command>) {
        use crate::commands::Command;
        match &self.updates.state {
            State::Idle | State::Available(_) => ("Check for Updates...".into(), Some(Command::CheckForUpdates)),
            State::Checking => ("Checking for Updates...".into(), None),
            State::Downloading(r) => (format!("Downloading Orbvane {}...", r.version), None),
            State::Ready(..) => ("Restart to Update (1)".into(), Some(Command::RestartToUpdate)),
            State::Installing => ("Installing Update...".into(), None),
        }
    }

    /// The gear menu at the bottom of the activity bar.
    pub(super) fn manage_menu(&mut self, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        use crate::commands::Command;
        let item = |label: &str, enabled: bool| PopupItem::Item { label: label.into(), enabled, checked: None };
        let run = PopupAction::Run;
        let sep = || (PopupItem::Separator, PopupAction::None);
        let (update, update_cmd) = self.update_menu_entry();
        let themes = vec![(item("Color Theme", true), run(Command::SelectTheme))];
        let entries = vec![
            (item("Command Palette...", true), run(Command::CommandPalette)),
            sep(),
            (item("Settings", true), run(Command::OpenSettings)),
            (item("Extensions", true), run(Command::ShowExtensions)),
            (item("Keyboard Shortcuts", true), run(Command::OpenKeybindings)),
            sep(),
            (item("Themes", true), PopupAction::Submenu(themes)),
            sep(),
            (item(&update, update_cmd.is_some()), update_cmd.map_or(PopupAction::None, run)),
        ];
        self.show_popup(entries, x, y);
    }

    /// Restart to Update: swaps the waiting update in, then quits and opens it.
    pub(super) fn restart_to_update(&mut self) {
        let (State::Ready(_, staged), Some(app)) = (self.updates.state.clone(), self.updates.app.clone()) else {
            self.notify(Severity::Info, "There's no update ready to install. Check for Updates... looks for one.", "", Vec::new(), None);
            return;
        };
        self.updates.state = State::Installing;
        self.update_spawn(move || Reply::Swapped(updater::swap(&staged, &app)));
    }

    /// Quitting with an update waiting installs it, when that needs no password.
    pub(super) fn install_update_on_quit(&mut self) {
        if let (State::Ready(_, staged), Some(app)) = (&self.updates.state, &self.updates.app) {
            if !updater::swap_needs_admin(app) {
                let _ = updater::swap(staged, app);
                self.updates.state = State::Idle;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::updater::testing::{fake_app, fake_dmg, version_of, TEST_REQUIREMENT};
    use extensions::gallery::testing::serve;

    fn wait_for(wb: &mut Workbench, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: toasts {:?}", wb.toast_list());
            std::thread::sleep(Duration::from_millis(10));
            wb.updates_tick();
        }
    }

    /// A stand-in for GitHub's latest-release API: release `version` with its disk image.
    fn releases(dir: &std::path::Path, version: &str) -> String {
        let dmg = std::fs::read(fake_dmg(dir, version)).unwrap();
        let sha = extensions::gallery::sha256_of(&dir.join(format!("Orbvane-{version}.dmg"))).unwrap();
        let version = version.to_string();
        let base = serve(move |base| {
            let release = serde_json::json!({
                "tag_name": format!("v{version}"),
                "html_url": format!("{base}/releases/tag/v{version}"),
                "assets": [{ "name": format!("Orbvane-{version}.dmg"), "browser_download_url": format!("{base}/Orbvane-{version}.dmg"), "digest": format!("sha256:{sha}") }]
            });
            vec![("/latest".into(), release.to_string().into_bytes()), (format!("/Orbvane-{version}.dmg"), dmg)]
        });
        format!("{base}/latest")
    }

    /// The gear menu's last entry (the update one): its label and whether it's enabled.
    fn gear_update_entry(wb: &mut Workbench) -> (String, bool) {
        wb.take_effects();
        wb.manage_menu(0.0, 0.0);
        let items = wb.take_effects().into_iter().find_map(|e| match e {
            Effect::Popup { items, .. } => Some(items),
            _ => None,
        });
        match items.and_then(|i| i.last().cloned()) {
            Some(super::super::PopupItem::Item { label, enabled, .. }) => (label, enabled),
            other => panic!("unexpected gear menu: {other:?}"),
        }
    }

    fn last_toast(wb: &Workbench) -> (String, Vec<String>) {
        wb.toast_list().last().map(|(_, m, a)| (m.clone(), a.clone())).unwrap_or_default()
    }

    #[test]
    fn downloads_verifies_and_installs_an_update() {
        let dir = std::env::temp_dir().join(format!("orbvane-updates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(None, &[], Arc::new(|| {}));
        let app = fake_app(&dir.join("Applications"), updater::current_version());
        wb.updates.app = Some(app.clone());
        wb.updates.dir = dir.join("Update");
        wb.updates.requirement = Some(TEST_REQUIREMENT.into());

        assert_eq!(gear_update_entry(&mut wb), ("Check for Updates...".into(), true));
        assert!(!wb.update_ready());

        // Nothing newer.
        wb.updates.url = releases(&dir, updater::current_version());
        wb.check_for_updates_command();
        wait_for(&mut wb, "the check", |wb| wb.updates.state == State::Idle);
        assert_eq!(last_toast(&wb).0, "There are currently no updates available.");

        // A newer release: downloaded, checked and staged, then offered.
        wb.updates.url = releases(&dir, "99.0.0");
        wb.check_for_updates_command();
        wait_for(&mut wb, "the download", |wb| matches!(wb.updates.state, State::Ready(..)));
        let (message, actions) = last_toast(&wb);
        assert_eq!(message, "Orbvane 99.0.0 is ready to install. Restart Orbvane to apply the update.");
        assert_eq!(actions, ["Update Now", "Later", "Release Notes"]);
        assert!(!wb.updates.dir.join("Orbvane-99.0.0.dmg").exists(), "the download is removed once staged");
        // The gear gets a badge and offers the restart.
        assert!(wb.update_ready());
        assert_eq!(gear_update_entry(&mut wb), ("Restart to Update (1)".into(), true));
        // Check for Updates... again just offers it again (one toast).
        wb.check_for_updates_command();
        assert_eq!(wb.toast_list().iter().filter(|(_, m, _)| m.contains("ready to install")).count(), 1);

        // "Later", then quitting installs it.
        let id = wb.toast_list().last().unwrap().0;
        wb.close_toast(id, Some(1));
        assert_eq!(version_of(&app), updater::current_version());
        wb.install_update_on_quit();
        assert_eq!(version_of(&app), "99.0.0");
        assert_eq!(wb.updates.state, State::Idle);

        // A copy that can't replace itself is sent to the download page.
        wb.updates.app = None;
        wb.check_for_updates_command();
        wait_for(&mut wb, "the check", |wb| matches!(wb.updates.state, State::Available(_)));
        let (message, actions) = last_toast(&wb);
        assert_eq!(message, "Orbvane 99.0.0 is available. This copy of Orbvane isn't an app bundle.");
        assert_eq!(actions, ["Download", "Later"]);

        // With updates off, Check for Updates... says so.
        let _ = wb.settings.set(settings::Scope::User, "update.mode", Some(serde_json::json!("none")));
        wb.check_for_updates_command();
        assert!(last_toast(&wb).0.starts_with("Updates are turned off."));
        let _ = wb.settings.set(settings::Scope::User, "update.mode", None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
