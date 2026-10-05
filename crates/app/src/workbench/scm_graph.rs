//! The Source Control view's GRAPH section, like the standard Source Control Graph: the history
//! of HEAD and its upstream with branch lanes (`scm::graph`), branch and tag labels, author
//! and subject. Clicking a commit lists the files it changed; clicking one of those opens what
//! the commit changed in it.

use std::collections::HashMap;
use std::path::PathBuf;

use render::{Canvas, Color, Rect, TextStyle};
use scm::graph::{GraphCommit, GraphRow, Segment};
use scm::FileStatus;

use super::sections::MIN_BODY;
use super::{Hit, Workbench, ROW_H, SMALL, UI};
use crate::diff_view::DiffSpec;
use crate::icons;

/// Width of a lane.
const LANE: f32 = 11.0;
/// Lanes drawn at most (more are clipped).
const MAX_LANES: usize = 12;

pub(super) struct ScmGraph {
    pub(super) open: bool,
    /// Body height (None: 40% of the view).
    pub(super) height: Option<f32>,
    commits: Vec<GraphCommit>,
    rows: Vec<GraphRow>,
    requested: bool,
    loaded: bool,
    /// Expanded commits and their files (None until loaded).
    expanded: HashMap<String, Option<Vec<(char, PathBuf)>>>,
    scroll: f32,
    body: Rect,
    /// The visible list: (commit, file within it).
    list: Vec<(usize, Option<usize>)>,
}

impl Default for ScmGraph {
    fn default() -> Self {
        Self {
            open: true,
            height: None,
            commits: Vec::new(),
            rows: Vec::new(),
            requested: false,
            loaded: false,
            expanded: HashMap::new(),
            scroll: 0.0,
            body: Rect::default(),
            list: Vec::new(),
        }
    }
}

/// A status letter's color, like the change lists'.
fn letter_status(letter: char) -> FileStatus {
    match letter {
        'A' => FileStatus::Added,
        'D' => FileStatus::Deleted,
        'R' => FileStatus::Renamed,
        'C' => FileStatus::Copied,
        _ => FileStatus::Modified,
    }
}

/// A straight line from (x0, y0) to (x1, y1), `w` thick.
fn line(c: &mut Canvas, (x0, y0): (f32, f32), (x1, y1): (f32, f32), w: f32, color: Color) {
    if (x0 - x1).abs() < 0.5 {
        c.fill(Rect::new(x0 - w / 2.0, y0.min(y1), w, (y1 - y0).abs()), color);
        return;
    }
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * 1.5).ceil() as usize;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        c.fill(Rect::new(x0 + (x1 - x0) * t - w / 2.0, y0 + (y1 - y0) * t - w / 2.0, w, w), color);
    }
}

impl Workbench {
    /// The color of graph color index `i`: the current branch in the ref color, other lanes
    /// in scmGraph.foreground1..5.
    fn graph_color(&self, i: usize) -> Color {
        if i == 0 {
            self.color("scmGraph.historyItemRefColor")
        } else {
            self.color(&format!("scmGraph.foreground{}", (i - 1) % 5 + 1))
        }
    }

    /// Asks for the history (when the section is open and it's stale). Called every frame.
    pub(super) fn scm_graph_tick(&mut self) {
        let g = &mut self.scm_graph;
        if g.open && !g.requested {
            if let Some(repo) = &self.repo {
                repo.send(scm::Job::Graph);
                g.requested = true;
            }
        }
    }

    /// Git state changed: new commits, moved branches.
    pub(super) fn refresh_scm_graph(&mut self) {
        self.scm_graph.requested = false;
    }

    pub(super) fn scm_graph_arrived(&mut self, commits: Vec<GraphCommit>) {
        let g = &mut self.scm_graph;
        g.rows = scm::graph::layout(&commits);
        g.commits = commits;
        g.loaded = true;
        let known: Vec<String> = g.commits.iter().map(|c| c.hash.clone()).collect();
        g.expanded.retain(|h, _| known.contains(h));
    }

    pub(super) fn commit_files_arrived(&mut self, hash: String, files: Vec<(char, PathBuf)>) {
        if let Some(slot) = self.scm_graph.expanded.get_mut(&hash) {
            *slot = Some(files);
        }
    }

    pub(super) fn toggle_scm_graph(&mut self) {
        self.scm_graph.open = !self.scm_graph.open;
    }

    pub(super) fn scm_graph_scroll(&mut self, dy: f32) {
        let g = &mut self.scm_graph;
        let max = (g.list.len() as f32 * ROW_H - g.body.h).max(0.0);
        g.scroll = (g.scroll - dy).clamp(0.0, max);
    }

    pub(super) fn drag_scm_graph_sash(&mut self, y: f32) {
        let bottom = self.scm_graph.body.bottom();
        self.scm_graph.height = Some((bottom - y - ROW_H).max(MIN_BODY));
    }

    /// The graph's body height within `avail` (the list area below the commit button).
    pub(super) fn scm_graph_height(&self, avail: f32) -> f32 {
        if !self.scm_graph.open {
            return 0.0;
        }
        let max = (avail - ROW_H - MIN_BODY).max(MIN_BODY);
        self.scm_graph.height.unwrap_or(avail * 0.4).clamp(MIN_BODY.min(max), max)
    }

    pub(super) fn click_scm_graph_row(&mut self, i: usize) {
        let Some(&(ci, file)) = self.scm_graph.list.get(i) else { return };
        let hash = self.scm_graph.commits[ci].hash.clone();
        match file {
            None => {
                if self.scm_graph.expanded.remove(&hash).is_none() {
                    self.scm_graph.expanded.insert(hash.clone(), None);
                    if let Some(repo) = &self.repo {
                        repo.send(scm::Job::CommitFiles(hash));
                    }
                }
            }
            Some(fi) => {
                let path = self.scm_graph.expanded.get(&hash).and_then(|f| f.as_ref()).and_then(|f| f.get(fi)).map(|(_, p)| p.clone());
                if let Some(path) = path {
                    self.open_diff(DiffSpec { path, staged: false, revision: Some(hash), left_file: None });
                }
            }
        }
    }

    pub(super) fn draw_scm_graph(&mut self, c: &mut Canvas, r: Rect) {
        self.scm_graph.body = r;
        self.hits.push((r, Hit::ScmGraphBody));
        let fg = self.color_or("sideBar.foreground", "foreground");
        let style = TextStyle::ui(UI, fg);
        if self.scm_graph.commits.is_empty() {
            let msg = if self.scm_graph.loaded { "No history yet." } else { "Loading..." };
            c.text(r.x + 20.0, r.y + 8.0, msg, &style);
            return;
        }
        // The visible list: commits, and the files of expanded ones.
        let g = &self.scm_graph;
        let mut list = Vec::new();
        for (ci, commit) in g.commits.iter().enumerate() {
            list.push((ci, None));
            if let Some(Some(files)) = g.expanded.get(&commit.hash) {
                list.extend((0..files.len()).map(|fi| (ci, Some(fi))));
            }
        }
        let max = (list.len() as f32 * ROW_H - r.h).max(0.0);
        let scroll = g.scroll.min(max);
        let lanes = g.rows.iter().map(|row| row.width).max().unwrap_or(1).min(MAX_LANES);
        let graph_x = r.x + 12.0;
        let text_x = graph_x + lanes as f32 * LANE + 8.0;
        let dim = TextStyle::ui(SMALL, self.color("descriptionForeground"));
        let pill_fg = self.color("scmGraph.historyItemHoverLabelForeground");
        let root = self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
        let first = (scroll / ROW_H) as usize;
        let visible = (r.h / ROW_H).ceil() as usize + 1;
        let mut hits = Vec::new();
        c.push_clip(r);
        for (i, &(ci, file)) in list.iter().enumerate().skip(first).take(visible) {
            let y = r.y + i as f32 * ROW_H - scroll;
            let rr = Rect::new(r.x, y, r.w, ROW_H);
            if self.hover_hit == Some(Hit::ScmGraphRow(i)) {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let row = &self.scm_graph.rows[ci];
            let lane_x = |l: usize| graph_x + l as f32 * LANE + LANE / 2.0;
            let mid = y + ROW_H / 2.0;
            c.push_clip(Rect::new(graph_x, y, lanes as f32 * LANE, ROW_H));
            match file {
                None => {
                    let segs: Vec<(Segment, bool)> = row.top.iter().map(|s| (*s, true)).chain(row.bottom.iter().map(|s| (*s, false))).collect();
                    for (s, top) in segs {
                        let color = self.graph_color(s.color);
                        if top {
                            line(c, (lane_x(s.from), y), (lane_x(s.to), mid), 1.5, color);
                        } else {
                            line(c, (lane_x(s.from), mid), (lane_x(s.to), y + ROW_H), 1.5, color);
                        }
                    }
                    let commit = &self.scm_graph.commits[ci];
                    let color = self.graph_color(row.color);
                    let dot = Rect::new(lane_x(row.lane) - 4.0, mid - 4.0, 8.0, 8.0);
                    if commit.refs.iter().any(|r| r.starts_with("HEAD")) {
                        // HEAD: a ring.
                        c.bordered(dot.inset(-1.0, -1.0), self.color("sideBar.background"), color, 2.0, 5.0);
                    } else {
                        c.fill_rounded(dot, color, 4.0);
                    }
                }
                Some(_) => {
                    // The lanes continuing below the commit pass through its files.
                    for s in &row.bottom {
                        line(c, (lane_x(s.to), y), (lane_x(s.to), y + ROW_H), 1.5, self.graph_color(s.color));
                    }
                }
            }
            c.pop_clip();
            let commit = &self.scm_graph.commits[ci];
            match file {
                None => {
                    // Labels at the right: branches and tags as pills, HEAD's branch first.
                    let mut pills = Vec::new();
                    let mut right = rr.right() - 8.0;
                    for (name, key) in ref_labels(&commit.refs) {
                        let tw = c.measure(name, &dim) + 12.0;
                        if right - tw < text_x + 60.0 {
                            break;
                        }
                        pills.push((name, key, tw));
                        right -= tw + 4.0;
                    }
                    let mut x = right + 4.0;
                    for (name, key, tw) in pills {
                        let pill = Rect::new(x, y + 3.0, tw, ROW_H - 6.0);
                        c.fill_rounded(pill, self.color(key), 7.0);
                        c.text_in(Rect::new(pill.x + 6.0, y, tw, ROW_H), name, &dim.color(pill_fg));
                        x += tw + 4.0;
                    }
                    let w = c.text_fit(Rect::new(text_x, y, right - text_x - 4.0, ROW_H), &commit.subject, &style);
                    if text_x + w + 8.0 < right {
                        c.text_fit(Rect::new(text_x + w + 8.0, y, right - text_x - w - 12.0, ROW_H), &commit.author, &dim);
                    }
                }
                Some(fi) => {
                    let Some(Some(files)) = self.scm_graph.expanded.get(&commit.hash) else { continue };
                    let (letter, path) = &files[fi];
                    let x = text_x + 8.0;
                    c.icon(&icons::FILE, x, y + 3.0, 16.0, super::file_color(path));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let dir = path.parent().and_then(|p| p.strip_prefix(&root).ok()).map(|p| p.display().to_string()).unwrap_or_default();
                    let color = super::scm_view::status_color(&self.theme, letter_status(*letter));
                    let nw = c.text_in(Rect::new(x + 20.0, y, rr.right() - x - 48.0, ROW_H), &name, &style.color(color));
                    c.text_in(Rect::new(x + 26.0 + nw, y, (rr.right() - x - 54.0 - nw).max(0.0), ROW_H), &dir, &dim);
                    c.text_in(Rect::new(rr.right() - 22.0, y, 14.0, ROW_H), &letter.to_string(), &style.color(color));
                }
            }
            hits.push((rr.intersect(&r), Hit::ScmGraphRow(i)));
        }
        c.pop_clip();
        self.scm_graph.list = list;
        self.scm_graph.scroll = scroll;
        self.hits.extend(hits);
    }
}

/// A commit's refs (full names, from `git log --decorate=full`) as labels with their color keys,
/// in the order we show them: the checked-out branch, other branches, tags, then remote
/// branches. A remote branch whose local branch sits on the same commit, and `origin/HEAD`, are
/// left out.
fn ref_labels(refs: &[String]) -> Vec<(&str, &'static str)> {
    let heads: Vec<&str> = refs.iter().filter_map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r).strip_prefix("refs/heads/")).collect();
    let mut labels: Vec<(u8, &str, &'static str)> = Vec::new();
    for r in refs {
        let (checked_out, full) = match r.strip_prefix("HEAD -> ") {
            Some(f) => (true, f),
            None => (false, r.as_str()),
        };
        if let Some(t) = full.strip_prefix("tag: refs/tags/") {
            labels.push((2, t, "scmGraph.historyItemBaseRefColor"));
        } else if let Some(b) = full.strip_prefix("refs/heads/") {
            labels.push((if checked_out { 0 } else { 1 }, b, "scmGraph.historyItemRefColor"));
        } else if let Some(rb) = full.strip_prefix("refs/remotes/") {
            let branch = rb.split_once('/').map_or(rb, |(_, b)| b);
            if branch != "HEAD" && !heads.contains(&branch) {
                labels.push((3, rb, "scmGraph.historyItemRemoteRefColor"));
            }
        }
    }
    labels.sort_by_key(|l| l.0);
    labels.into_iter().map(|(_, n, k)| (n, k)).collect()
}

#[cfg(test)]
mod tests {
    use super::ref_labels;

    #[test]
    fn labels_put_the_checked_out_branch_first() {
        let refs: Vec<String> =
            ["HEAD -> refs/heads/main", "refs/remotes/origin/main", "refs/remotes/origin/topic", "refs/remotes/origin/HEAD", "refs/heads/topic", "tag: refs/tags/v1", "refs/remotes/origin/wip", "refs/heads/feat/x"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let names: Vec<&str> = ref_labels(&refs).into_iter().map(|l| l.0).collect();
        assert_eq!(names, ["main", "topic", "feat/x", "v1", "origin/wip"]);
    }
}
