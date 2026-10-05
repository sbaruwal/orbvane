//! The Timeline (a tab of the secondary side bar), like the standard with its Git History source: the commits
//! that changed the active file, newest first, with their authors and ages. Clicking one opens
//! what that commit changed in the file as a diff.

use std::path::{Path, PathBuf};

use render::{Canvas, Rect, TextStyle};

use super::{Hit, Workbench, ROW_H, SMALL, UI};
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
    scroll: f32,
    body: Rect,
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
        let max = (t.entries.len() as f32 * ROW_H - t.body.h).max(0.0);
        t.scroll = (t.scroll - dy).clamp(0.0, max);
    }

    /// A commit was clicked: what it changed in the file.
    pub(super) fn open_timeline_entry(&mut self, i: usize) {
        let (Some(path), Some(entry)) = (self.timeline.path.clone(), self.timeline.entries.get(i)) else { return };
        let revision = Some(entry.hash.clone());
        self.open_diff(DiffSpec { path, staged: false, revision, left_file: None });
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
            (Some(_), true) if t.entries.is_empty() => Some("No timeline information was provided.".to_string()),
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
        for i in first..(first + visible).min(t.entries.len()) {
            let e = &t.entries[i];
            let y = r.y + i as f32 * ROW_H - t.scroll;
            let rr = Rect::new(r.x, y, r.w, ROW_H);
            if self.hover_hit == Some(Hit::TimelineRow(i)) {
                c.fill_rounded(super::row_pill(rr), self.theme.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            c.icon(&icons::GIT_COMMIT, rr.x + 20.0, y + 3.0, 16.0, icon_fg);
            let when = age(now - e.time);
            let when_w = c.measure(&when, &desc);
            c.text_in(Rect::new(rr.right() - when_w - 12.0, y, when_w + 2.0, ROW_H), &when, &desc);
            let x = rr.x + 42.0;
            let right = rr.right() - when_w - 20.0;
            let w = c.text_fit(Rect::new(x, y, (right - x).max(0.0), ROW_H), &e.subject, &style);
            if x + w + 40.0 < right {
                c.text_fit(Rect::new(x + w + 8.0, y, right - x - w - 8.0, ROW_H), &e.author, &desc);
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
