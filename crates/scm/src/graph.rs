//! The commit graph of the Source Control Graph view: which lane each commit sits in and the
//! lines connecting commits to their parents, computed from commits in topological order (the
//! usual "lanes" algorithm, as in `git log --graph`).

/// A commit to lay out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCommit {
    pub hash: String,
    pub parents: Vec<String>,
    /// Branch and tag names pointing at it ("HEAD -> main", "origin/main", "tag: v1").
    pub refs: Vec<String>,
    pub author: String,
    pub time: i64,
    pub subject: String,
}

/// A line within a row: from lane `from` to lane `to`, drawn in color `color` (an index into
/// the graph palette). Top lines run from the row's top edge to its middle, bottom lines from
/// the middle to the bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub from: usize,
    pub to: usize,
    pub color: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// The lane of the commit's dot, and its color.
    pub lane: usize,
    pub color: usize,
    pub top: Vec<Segment>,
    pub bottom: Vec<Segment>,
    /// Lanes in use in this row (for the graph's width).
    pub width: usize,
}

/// Lays out `commits` (newest first, parents after children).
pub fn layout(commits: &[GraphCommit]) -> Vec<GraphRow> {
    // Each lane waits for a commit (by hash) and has a color.
    let mut lanes: Vec<Option<(String, usize)>> = Vec::new();
    let mut next_color = 0;
    let mut new_color = || {
        next_color += 1;
        next_color - 1
    };
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let waiting: Vec<usize> = (0..lanes.len()).filter(|&i| lanes[i].as_ref().is_some_and(|(h, _)| *h == c.hash)).collect();
        let (lane, color) = match waiting.first() {
            Some(&i) => (i, lanes[i].as_ref().unwrap().1),
            None => {
                let color = new_color();
                let i = lanes.iter().position(Option::is_none).unwrap_or_else(|| {
                    lanes.push(None);
                    lanes.len() - 1
                });
                (i, color)
            }
        };
        // Lines coming in: lanes waiting for this commit converge on its dot; the others pass.
        let top: Vec<Segment> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().map(|(h, col)| Segment { from: i, to: if *h == c.hash { lane } else { i }, color: *col }))
            .collect();
        for &i in &waiting {
            lanes[i] = None;
        }
        // Lines going out: to each parent's lane (the first parent continues this lane unless
        // another lane already waits for it).
        let mut bottom = Vec::new();
        for (k, p) in c.parents.iter().enumerate() {
            let waits = lanes.iter().position(|l| l.as_ref().is_some_and(|(h, _)| h == p));
            // A lane to the right already waits for our first parent (a side branch listed
            // first): pull it into this lane, so the parent stays leftmost like `git log --graph`.
            if let Some(j) = waits.filter(|&j| k == 0 && j > lane && lanes[lane].is_none()) {
                let (_, col) = lanes[j].take().unwrap();
                bottom.push(Segment { from: j, to: lane, color: col });
                lanes[lane] = Some((p.clone(), color));
                bottom.push(Segment { from: lane, to: lane, color });
                continue;
            }
            if let Some(j) = waits {
                bottom.push(Segment { from: lane, to: j, color: lanes[j].as_ref().unwrap().1 });
                continue;
            }
            let (j, col) = if k == 0 && lanes[lane].is_none() {
                (lane, color)
            } else {
                let j = lanes.iter().position(Option::is_none).unwrap_or_else(|| {
                    lanes.push(None);
                    lanes.len() - 1
                });
                (j, new_color())
            };
            lanes[j] = Some((p.clone(), col));
            bottom.push(Segment { from: lane, to: j, color: col });
        }
        for (i, l) in lanes.iter().enumerate() {
            if let Some((_, col)) = l {
                if !bottom.iter().any(|s| s.to == i) {
                    bottom.push(Segment { from: i, to: i, color: *col });
                }
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let width = top.iter().chain(&bottom).map(|s| s.from.max(s.to) + 1).max().unwrap_or(0).max(lane + 1);
        rows.push(GraphRow { lane, color, top, bottom, width });
    }
    rows
}

/// Parses `git log --decorate=full --format=%H%x00%P%x00%D%x00%an%x00%at%x00%s` output (refs
/// are full names: `HEAD -> refs/heads/main`, `refs/remotes/origin/main`, `tag: refs/tags/v1`).
pub fn parse_commits(text: &str) -> Vec<GraphCommit> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(6, '\0');
            let hash = f.next()?.to_string();
            let parents = f.next()?.split_whitespace().map(str::to_string).collect();
            let refs = f.next()?.split(", ").filter(|r| !r.is_empty()).map(str::to_string).collect();
            let author = f.next()?.to_string();
            let time = f.next()?.parse().ok()?;
            let subject = f.next().unwrap_or("").to_string();
            Some(GraphCommit { hash, parents, refs, author, time, subject })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(hash: &str, parents: &[&str]) -> GraphCommit {
        GraphCommit { hash: hash.into(), parents: parents.iter().map(|p| p.to_string()).collect(), refs: Vec::new(), author: String::new(), time: 0, subject: String::new() }
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let rows = layout(&[c("c", &["b"]), c("b", &["a"]), c("a", &[])]);
        assert!(rows.iter().all(|r| r.lane == 0 && r.width == 1));
        assert!(rows[2].bottom.is_empty());
    }

    #[test]
    fn a_merge_opens_and_closes_a_lane() {
        // m merges f (feature) into x (main); both come from a.
        let rows = layout(&[c("m", &["x", "f"]), c("x", &["a"]), c("f", &["a"]), c("a", &[])]);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].bottom.iter().map(|s| (s.from, s.to)).collect::<Vec<_>>(), [(0, 0), (0, 1)]);
        assert_eq!(rows[1].lane, 0);
        assert_eq!(rows[2].lane, 1);
        // f's lane joins the lane already waiting for a, right below f.
        assert!(rows[2].bottom.iter().any(|s| s.from == 1 && s.to == 0));
        assert_eq!(rows[3].lane, 0);
        assert_eq!(rows[3].width, 1);
        assert_ne!(rows[0].bottom[1].color, rows[0].color);
    }

    #[test]
    fn a_side_branch_listed_first_joins_the_main_lane() {
        // m merges f into x, and f comes before x (it's newer).
        let rows = layout(&[c("m", &["x", "f"]), c("f", &["a"]), c("x", &["a"]), c("a", &[])]);
        assert_eq!((rows[1].lane, rows[2].lane), (1, 0));
        // At x, f's lane (waiting for a) moves over into x's lane.
        assert!(rows[2].bottom.iter().any(|s| s.from == 1 && s.to == 0));
        assert_eq!(rows[3].lane, 0);
        assert_eq!(rows[3].color, rows[0].color);
        assert_eq!(rows[3].width, 1);
    }

    #[test]
    fn parses_log_records() {
        let log = parse_commits("h1\0p1 p2\0HEAD -> main, origin/main\0Ann\017\0Merge x\nh2\0\0\0Bo\016\0Init\n");
        assert_eq!(log[0].parents, ["p1", "p2"]);
        assert_eq!(log[0].refs, ["HEAD -> main", "origin/main"]);
        assert!(log[1].parents.is_empty() && log[1].refs.is_empty());
    }
}
