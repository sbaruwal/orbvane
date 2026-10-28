//! The Timeline (a tab of the secondary side bar): the commits that changed the active file
//! and its local history (a copy kept at each save, `crate::local_history`), newest first.
//! Clicking a commit opens what it changed in the file; clicking a saved copy compares it with
//! the file. A copy's menu restores it.

use std::path::{Path, PathBuf};

use render::{Canvas, Rect, TextStyle};

use super::preferences::PopupAction;
use super::{Hit, PopupItem, Workbench, ROW_H, SMALL, UI};
use crate::diff_view::DiffSpec;
use crate::icons;

#[derive(Default)]
pub(super) struct Timeline {
    /// Whether it's shown (the secondary side bar's Timeline tab); history is fetched only then.
    pub(super) open: bool,
    /// The file shown, and whether its history has arrived.
    path: Option<PathBuf>,
    loaded: bool,
    entries: Vec<scm::LogEntry>,
    /// The file's local history, newest first.
    local: Vec<crate::local_history::Entry>,
    scroll: f32,
    body: Rect,
}

/// A row: a commit or a saved copy (indexes into `entries` and `local`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Commit(usize),
    Local(usize),
}

/// What a row's menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimelineAction {
    Open,
    Restore,
}

impl Timeline {
    /// Commits and copies merged, newest first.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<(i64, Row)> = self.entries.iter().enumerate().map(|(i, e)| (e.time * 1000, Row::Commit(i))).collect();
        rows.extend(self.local.iter().enumerate().map(|(i, e)| (e.timestamp, Row::Local(i))));
        rows.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        rows.into_iter().map(|(_, r)| r).collect()
    }
}

/// "now", "5 mins", "3 hrs", "2 days", "3 wks", "4 mos", "2 yrs", like the standard timeline.
pub(super) fn age(seconds: i64) -> String {
    let s = seconds.max(0);
    let (n, unit) = match s {
        0..=59 => return "now".into(),
        60..=3599 => (s / 60, "min"),
        3600..=86_399 => (s / 3600, "hr"),
        86_400..=604_799 => (s / 86_400, "day"),
        604_800..=2_629_799 => (s / 604_800, "wk"),
        2_629_800..=31_557_599 => (s / 2_629_800, "mo"),
        _ => (s / 31_557_600, "yr"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

impl Workbench {
    /// Shows or hides it (following the secondary side bar).
    pub(super) fn set_timeline_open(&mut self, open: bool) {
        if open != self.timeline.open {
            self.timeline.open = open;
            self.timeline.path = None; // ask again
        }
    }

    /// Follows the active file; asks git for its history while the section is open.
    pub(super) fn timeline_tick(&mut self) {
        if !self.timeline.open {
            return;
        }
        let path = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf));
        if path == self.timeline.path {
            return;
        }
        let t = &mut self.timeline;
        t.path = path.clone();
        t.entries.clear();
        t.local = path.as_deref().map(|p| crate::local_history::entries(&crate::local_history::root(), p)).unwrap_or_default();
        t.loaded = false;
        t.scroll = 0.0;
        match (path, &self.repo) {
            (Some(p), Some(repo)) if p.starts_with(&repo.root) => repo.send(scm::Job::FileLog(p)),
            _ => self.timeline.loaded = true,
        }
    }

    /// New commits (git status changed): the history may have grown.
    pub(super) fn refresh_timeline(&mut self) {
        self.timeline.path = None;
    }

    pub(super) fn timeline_arrived(&mut self, path: PathBuf, entries: Vec<scm::LogEntry>) {
        if self.timeline.path.as_ref() == Some(&path) {
            self.timeline.entries = entries;
            self.timeline.loaded = true;
        }
    }

    pub(super) fn timeline_scroll(&mut self, dy: f32) {
        let t = &mut self.timeline;
        let max = ((t.entries.len() + t.local.len()) as f32 * ROW_H - t.body.h).max(0.0);
        t.scroll = (t.scroll - dy).clamp(0.0, max);
    }

    /// Keeps the saved text in the file's local history (`workbench.localHistory.*`).
    pub(super) fn keep_local_copy(&mut self, path: &Path) {
        let root = crate::local_history::root();
        if !self.settings.bool("workbench.localHistory.enabled") || path.starts_with(&root) {
            return;
        }
        let Some(text) = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path)).map(|d| d.buffer.text()) else { return };
        if text.len() as f64 > self.settings.number("workbench.localHistory.maxFileSize") * 1024.0 {
            return;
        }
        let max = self.settings.number("workbench.localHistory.maxFileEntries") as usize;
        if let Err(e) = crate::local_history::record(&root, path, &text, "File Saved", max) {
            eprintln!("local history: {}: {e}", path.display());
        }
        self.timeline_saved(path);
    }

    /// The file was saved: its local history grew.
    pub(super) fn timeline_saved(&mut self, path: &Path) {
        if self.timeline.path.as_deref() == Some(path) {
            self.timeline.local = crate::local_history::entries(&crate::local_history::root(), path);
        }
    }

    /// Row `i` was clicked: a commit's changes to the file, or a copy compared with the file.
    pub(super) fn open_timeline_entry(&mut self, i: usize) {
        let Some(path) = self.timeline.path.clone() else { return };
        match self.timeline.rows().get(i) {
            Some(&Row::Commit(c)) => {
                let revision = Some(self.timeline.entries[c].hash.clone());
                self.open_diff(DiffSpec { path, staged: false, revision, left_file: None });
            }
            Some(&Row::Local(l)) => {
                let copy = crate::local_history::file_of(&crate::local_history::root(), &path, &self.timeline.local[l]);
                self.open_diff(DiffSpec { path, staged: false, revision: None, left_file: Some(copy) });
            }
            None => {}
        }
    }

    /// Row `i`'s context menu.
    pub(super) fn timeline_menu(&mut self, i: usize, x: f32, y: f32) {
        let item = |label: &str| PopupItem::Item { label: label.into(), enabled: true, checked: None };
        let entries = match self.timeline.rows().get(i) {
            Some(Row::Commit(_)) => vec![(item("Open Changes"), PopupAction::Timeline(i, TimelineAction::Open))],
            Some(Row::Local(_)) => vec![
                (item("Compare with File"), PopupAction::Timeline(i, TimelineAction::Open)),
                (item("Restore Contents"), PopupAction::Timeline(i, TimelineAction::Restore)),
            ],
            None => return,
        };
        self.show_popup(entries, x, y);
    }

    pub(super) fn timeline_action(&mut self, i: usize, action: TimelineAction) {
        match action {
            TimelineAction::Open => self.open_timeline_entry(i),
            TimelineAction::Restore => self.restore_local_copy(i),
        }
    }

    /// Puts copy row `i` back as the file's contents: an edit of the open document (undoable,
    /// saved if it had no unsaved changes), else written to disk.
    fn restore_local_copy(&mut self, i: usize) {
        let Some(path) = self.timeline.path.clone() else { return };
        let Some(&Row::Local(l)) = self.timeline.rows().get(i) else { return };
        let copy = crate::local_history::file_of(&crate::local_history::root(), &path, &self.timeline.local[l]);
        let Ok(text) = std::fs::read_to_string(&copy) else { return };
        match self.agent_write(&path, &text) {
            Ok(()) => self.set_status_message(&format!("Restored {} from local history", self.display_path(&path))),
            Err(e) => {
                let msg = format!("Unable to restore '{}' ({e}).", self.display_path(&path));
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Local History").set_description(msg).show();
            }
        }
        self.timeline_saved(&path);
    }

    pub(super) fn draw_timeline(&mut self, c: &mut Canvas, r: Rect) {
        self.timeline.body = r;
        self.hits.push((r, Hit::TimelineBody));
        let fg = self.color_or("sideBar.foreground", "foreground");
        let style = TextStyle::ui(UI, fg);
        let t = &self.timeline;
        let message = match (&t.path, t.loaded) {
            (None, _) => Some("The active editor cannot provide timeline information.".to_string()),
            (Some(p), false) => Some(format!("Loading timeline for {}...", p.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()))),
            (Some(_), true) if t.entries.is_empty() && t.local.is_empty() => Some("No timeline information was provided.".to_string()),
            _ => None,
        };
        c.push_clip(r);
        if let Some(message) = message {
            let lines = super::intel::wrap(c, &message, &style, r.w - 40.0);
            for (i, line) in lines.iter().enumerate() {
                c.text(r.x + 20.0, r.y + 8.0 + i as f32 * 18.0, line, &style);
            }
            c.pop_clip();
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let desc = TextStyle::ui(SMALL, self.color("descriptionForeground"));
        let icon_fg = self.color("icon.foreground");
        let first = (t.scroll / ROW_H) as usize;
        let visible = (r.h / ROW_H).ceil() as usize + 1;
        let mut hits = Vec::new();
        let rows = t.rows();
        for (i, row) in rows.iter().enumerate().skip(first).take(visible) {
            let (icon, title, detail, time) = match *row {
                Row::Commit(c) => {
                    let e = &t.entries[c];
                    (&icons::GIT_COMMIT, e.subject.as_str(), e.author.as_str(), e.time)
                }
                Row::Local(l) => {
                    let e = &t.local[l];
                    (&icons::HISTORY, e.source.as_str(), "", e.timestamp / 1000)
                }
            };
            let y = r.y + i as f32 * ROW_H - t.scroll;
            let rr = Rect::new(r.x, y, r.w, ROW_H);
            if self.hover_hit == Some(Hit::TimelineRow(i)) {
                c.fill_rounded(super::row_pill(rr), self.theme.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            c.icon(icon, rr.x + 20.0, y + 3.0, 16.0, icon_fg);
            let when = age(now - time);
            let when_w = c.measure(&when, &desc);
            c.text_in(Rect::new(rr.right() - when_w - 12.0, y, when_w + 2.0, ROW_H), &when, &desc);
            let x = rr.x + 42.0;
            let right = rr.right() - when_w - 20.0;
            let w = c.text_fit(Rect::new(x, y, (right - x).max(0.0), ROW_H), title, &style);
            if !detail.is_empty() && x + w + 40.0 < right {
                c.text_fit(Rect::new(x + w + 8.0, y, right - x - w - 8.0, ROW_H), detail, &desc);
            }
            hits.push((rr.intersect(&r), Hit::TimelineRow(i)));
        }
        c.pop_clip();
        self.hits.extend(hits);
    }
}

#[cfg(test)]
mod tests {
    use super::age;

    #[test]
    fn formats_ages() {
        assert_eq!(age(10), "now");
        assert_eq!(age(60), "1 min");
        assert_eq!(age(3 * 3600), "3 hrs");
        assert_eq!(age(2 * 86_400), "2 days");
        assert_eq!(age(3 * 604_800), "3 wks");
        assert_eq!(age(400 * 86_400), "1 yr");
    }
}
