//! Three-way merge, like the standard merge editor model: the ranges of the common ancestor
//! (base) that either side changed, how to combine them, and what the result document
//! currently does with each of them.
//!
//! Input 1 is the incoming side (git's stage 3, "theirs") and input 2 the current side
//! (stage 2, "ours"). Texts are split into lines at `\n`; a range's state is
//! read back from the result text by diffing it against the base, so the user can also edit
//! the result by hand (a state no action produces is `Manual`).

use std::ops::Range;

/// A changed region: (lines of the base, lines of the input replacing them).
type Hunk = (Range<usize>, Range<usize>);

/// A range of the base that one or both inputs changed (touching changes are joined).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRange {
    pub base: Range<usize>,
    pub input1: Range<usize>,
    pub input2: Range<usize>,
    /// Each input's changes inside the range.
    diffs1: Vec<Hunk>,
    diffs2: Vec<Hunk>,
}

impl MergeRange {
    pub fn changed(&self, input: u8) -> bool {
        !self.diffs(input).is_empty()
    }

    fn diffs(&self, input: u8) -> &[Hunk] {
        if input == 1 { &self.diffs1 } else { &self.diffs2 }
    }

    /// The lines of `input` (1 or 2) this range covers.
    pub fn input(&self, input: u8) -> Range<usize> {
        if input == 1 { self.input1.clone() } else { self.input2.clone() }
    }

    /// Lines of `input` that its changes in this range added or replaced.
    pub fn changed_lines(&self, input: u8) -> impl Iterator<Item = Range<usize>> + '_ {
        self.diffs(input).iter().map(|(_, r)| r.clone())
    }
}

/// What the result does with a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The base's lines (neither change).
    Base,
    Input1,
    Input2,
    /// Both changes, `first` input first; `smart` interleaves them where they don't overlap
    ///, otherwise one follows the other ("Append").
    Both { first: u8, smart: bool },
    /// Edited by hand.
    Manual,
}

impl State {
    pub fn includes(self, input: u8) -> bool {
        match self {
            State::Input1 => input == 1,
            State::Input2 => input == 2,
            State::Both { .. } => true,
            State::Base | State::Manual => false,
        }
    }

    /// This state with `input` taken (appended after what's there) or dropped.
    pub fn with(self, input: u8, on: bool) -> State {
        let other = 3 - input;
        let has_other = self.includes(other);
        match (on, has_other) {
            (true, true) => State::Both { first: other, smart: false },
            (true, false) => if input == 1 { State::Input1 } else { State::Input2 },
            (false, true) => if other == 1 { State::Input1 } else { State::Input2 },
            (false, false) => State::Base,
        }
    }
}

/// Where a range is in the result text, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Result lines.
    pub lines: Range<usize>,
    /// The base lines those correspond to: the range's base lines, grown to take in edits
    /// that touch them.
    base: Range<usize>,
    pub state: State,
}

pub struct Merge {
    pub base: Vec<String>,
    pub input1: Vec<String>,
    pub input2: Vec<String>,
    pub ranges: Vec<MergeRange>,
}

fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect()
}

/// Groups the changes of both inputs into ranges of the base, joining changes that overlap
/// or touch, and works out each range's lines in both inputs.
fn modified_ranges(h1: &[Hunk], h2: &[Hunk]) -> Vec<MergeRange> {
    let mut all: Vec<(u8, &Hunk)> = h1.iter().map(|h| (1, h)).chain(h2.iter().map(|h| (2, h))).collect();
    all.sort_by_key(|(side, (b, _))| (b.start, b.end, *side));
    let mut out: Vec<MergeRange> = Vec::new();
    // How many lines each input has gained before the current position.
    let (mut d1, mut d2) = (0isize, 0isize);
    let mut i = 0;
    while i < all.len() {
        let mut base = all[i].1 .0.clone();
        let mut j = i + 1;
        while j < all.len() && all[j].1 .0.start <= base.end {
            base.end = base.end.max(all[j].1 .0.end);
            j += 1;
        }
        let group = &all[i..j];
        let pick = |side: u8| group.iter().filter(|(s, _)| *s == side).map(|(_, h)| (*h).clone()).collect::<Vec<Hunk>>();
        let (diffs1, diffs2) = (pick(1), pick(2));
        let gain = |d: &[Hunk]| d.iter().map(|(b, r)| r.len() as isize - b.len() as isize).sum::<isize>();
        let span = |d: isize, g: isize| (base.start as isize + d) as usize..(base.end as isize + d + g) as usize;
        let (g1, g2) = (gain(&diffs1), gain(&diffs2));
        out.push(MergeRange { base: base.clone(), input1: span(d1, g1), input2: span(d2, g2), diffs1, diffs2 });
        (d1, d2) = (d1 + g1, d2 + g2);
        i = j;
    }
    out
}

impl Merge {
    pub fn new(base: &str, input1: &str, input2: &str) -> Self {
        let ranges = modified_ranges(&scm::hunks(base, input1), &scm::hunks(base, input2));
        Self { base: lines(base), input1: lines(input1), input2: lines(input2), ranges }
    }

    fn input_lines(&self, input: u8) -> &[String] {
        if input == 1 { &self.input1 } else { &self.input2 }
    }

    /// Both inputs made the same change.
    fn is_equal_change(&self, i: usize) -> bool {
        let r = &self.ranges[i];
        self.input1[r.input1.clone()] == self.input2[r.input2.clone()]
    }

    /// Both inputs changed the range, differently.
    pub fn is_conflicting(&self, i: usize) -> bool {
        let r = &self.ranges[i];
        r.changed(1) && r.changed(2) && !self.is_equal_change(i)
    }

    /// Both inputs' changes applied to the base, `first`'s first where they insert at the same
    /// place; None when they overlap.
    fn smart_combine(&self, i: usize, first: u8) -> Option<Vec<String>> {
        let r = &self.ranges[i];
        let mut all: Vec<(u8, &Hunk)> = r.diffs1.iter().map(|h| (1, h)).chain(r.diffs2.iter().map(|h| (2, h))).collect();
        all.sort_by_key(|(side, (b, _))| (b.start, *side != first, b.end));
        if all.windows(2).any(|w| w[0].1 .0.end > w[1].1 .0.start) {
            return None;
        }
        let mut out = Vec::new();
        let mut at = r.base.start;
        for (side, (b, lines)) in all {
            out.extend_from_slice(&self.base[at..b.start]);
            out.extend_from_slice(&self.input_lines(side)[lines.clone()]);
            at = b.end;
        }
        out.extend_from_slice(&self.base[at..r.base.end]);
        Some(out)
    }

    pub fn can_be_combined(&self, i: usize) -> bool {
        self.smart_combine(i, 1).is_some()
    }

    /// The two orders of combining give different results.
    pub fn is_order_relevant(&self, i: usize) -> bool {
        self.smart_combine(i, 1) != self.smart_combine(i, 2)
    }

    /// The range's lines in state `state` (None for `Manual`).
    pub fn candidate(&self, i: usize, state: State) -> Option<Vec<String>> {
        let r = &self.ranges[i];
        Some(match state {
            State::Base => self.base[r.base.clone()].to_vec(),
            State::Input1 => self.input1[r.input1.clone()].to_vec(),
            State::Input2 => self.input2[r.input2.clone()].to_vec(),
            State::Both { first, smart } => {
                let combined = if smart { self.smart_combine(i, first) } else { None };
                combined.unwrap_or_else(|| {
                    let second = 3 - first;
                    let mut v = self.input_lines(first)[r.input(first)].to_vec();
                    v.extend_from_slice(&self.input_lines(second)[r.input(second)]);
                    v
                })
            }
            State::Manual => return None,
        })
    }

    /// What a range starts as: the one change there is (or the change both made), and the
    /// base for conflicts. The flag says whether it counts as handled.
    pub fn default_state(&self, i: usize) -> (State, bool) {
        let r = &self.ranges[i];
        if !r.changed(1) {
            (State::Input2, true)
        } else if !r.changed(2) || self.is_equal_change(i) {
            (State::Input1, true)
        } else {
            (State::Base, false)
        }
    }

    /// The base with every range in its default state: all changes that don't conflict.
    pub fn auto_merged(&self) -> String {
        let mut out: Vec<String> = Vec::with_capacity(self.base.len());
        let mut at = 0;
        for (i, r) in self.ranges.iter().enumerate() {
            out.extend_from_slice(&self.base[at..r.base.start]);
            out.extend(self.candidate(i, self.default_state(i).0).unwrap_or_default());
            at = r.base.end;
        }
        out.extend_from_slice(&self.base[at..]);
        out.join("\n")
    }

    /// Where each range is in `result`, and its state there.
    pub fn regions(&self, result: &str) -> Vec<Region> {
        let res = lines(result);
        let hunks = scm::hunks(&self.base.join("\n"), &res.join("\n"));
        self.ranges
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (bs, be) = (r.base.start, r.base.end);
                let touches = |b: &Range<usize>| {
                    if b.is_empty() || bs == be {
                        b.start <= be && b.end >= bs
                    } else {
                        b.start < be && b.end > bs
                    }
                };
                let (mut before, mut inside) = (0isize, 0isize);
                let mut base = r.base.clone();
                for (b, n) in &hunks {
                    let gain = n.len() as isize - b.len() as isize;
                    if touches(b) {
                        base = base.start.min(b.start)..base.end.max(b.end);
                        inside += gain;
                    } else if b.end <= bs {
                        before += gain;
                    }
                }
                let start = (base.start as isize + before) as usize;
                let end = ((base.end as isize + before + inside).max(start as isize) as usize).min(res.len());
                let start = start.min(end);
                let got = &res[start..end];
                let (pre, post) = (&self.base[base.start..bs], &self.base[be..base.end]);
                let matches = |c: &[String]| got.len() == pre.len() + c.len() + post.len() && got[..pre.len()] == *pre && got[pre.len()..pre.len() + c.len()] == *c && got[pre.len() + c.len()..] == *post;
                let mut candidates = vec![State::Base, State::Input1, State::Input2];
                for first in [1, 2] {
                    if self.smart_combine(i, first).is_some() {
                        candidates.push(State::Both { first, smart: true });
                    }
                    candidates.push(State::Both { first, smart: false });
                }
                let state = candidates.into_iter().find(|&s| self.candidate(i, s).is_some_and(|c| matches(&c))).unwrap_or(State::Manual);
                Region { lines: start..end, base, state }
            })
            .collect()
    }

    /// The lines to put in `region` for range `i` to be in `state`: the candidate, plus the
    /// base lines the region took in around it.
    pub fn replacement(&self, i: usize, region: &Region, state: State) -> Option<Vec<String>> {
        let r = &self.ranges[i];
        let mut v = self.base[region.base.start..r.base.start].to_vec();
        v.extend(self.candidate(i, state)?);
        v.extend_from_slice(&self.base[r.base.end..region.base.end]);
        Some(v)
    }

    /// `result` with conflict markers around the ranges not handled: the current side (or
    /// the hand-edited result, so nothing is lost) and the incoming side, as git writes them.
    pub fn with_markers(&self, result: &str, unhandled: &[bool]) -> String {
        let res = lines(result);
        let regions = self.regions(result);
        let mut out: Vec<String> = Vec::with_capacity(res.len());
        let mut at = 0;
        for (i, reg) in regions.iter().enumerate() {
            if !unhandled.get(i).copied().unwrap_or(false) || reg.lines.start < at {
                continue;
            }
            let r = &self.ranges[i];
            out.extend_from_slice(&res[at..reg.lines.start]);
            out.push("<<<<<<< Current".into());
            if reg.state == State::Manual {
                out.extend_from_slice(&res[reg.lines.clone()]);
            } else {
                out.extend_from_slice(&self.input2[r.input2.clone()]);
            }
            out.push("=======".into());
            out.extend_from_slice(&self.input1[r.input1.clone()]);
            out.push(">>>>>>> Incoming".into());
            at = reg.lines.end;
        }
        out.extend_from_slice(&res[at..]);
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "a\nb\nc\nd\ne\nf\ng\n";
    // Incoming changes b and e; current changes b differently and g.
    const IN1: &str = "a\nB1\nc\nd\nE\nf\ng\n";
    const IN2: &str = "a\nB2\nc\nd\ne\nf\nG\n";

    #[test]
    fn finds_ranges_and_conflicts() {
        let m = Merge::new(BASE, IN1, IN2);
        let spans: Vec<_> = m.ranges.iter().map(|r| (r.base.clone(), r.input1.clone(), r.input2.clone())).collect();
        assert_eq!(spans, vec![(1..2, 1..2, 1..2), (4..5, 4..5, 4..5), (6..7, 6..7, 6..7)]);
        assert!(m.is_conflicting(0));
        assert!(!m.is_conflicting(1) && !m.is_conflicting(2));
        assert_eq!(m.default_state(1), (State::Input1, true));
        assert_eq!(m.default_state(2), (State::Input2, true));
        assert_eq!(m.auto_merged(), "a\nb\nc\nd\nE\nf\nG\n");
    }

    #[test]
    fn reads_states_back_from_the_result() {
        let m = Merge::new(BASE, IN1, IN2);
        let states = |text: &str| m.regions(text).iter().map(|r| r.state).collect::<Vec<_>>();
        assert_eq!(states(&m.auto_merged()), vec![State::Base, State::Input1, State::Input2]);
        assert_eq!(states("a\nB2\nc\nd\nE\nf\nG\n")[0], State::Input2);
        assert_eq!(states("a\nB1\nB2\nc\nd\nE\nf\nG\n")[0], State::Both { first: 1, smart: false });
        assert_eq!(states("a\nhand\nc\nd\nE\nf\nG\n")[0], State::Manual);
        // Lines inserted elsewhere move the regions after them.
        let regions = m.regions("x\na\nb\nc\nd\nE\nf\nG\n");
        assert_eq!(regions[1].lines, 5..6);
        assert_eq!(regions[1].state, State::Input1);
    }

    #[test]
    fn replacing_a_region_sets_its_state() {
        let m = Merge::new(BASE, IN1, IN2);
        let text = m.auto_merged();
        let reg = &m.regions(&text)[0];
        let mut res = lines(&text);
        res.splice(reg.lines.clone(), m.replacement(0, reg, State::Both { first: 2, smart: false }).unwrap());
        let text = res.join("\n");
        assert_eq!(text, "a\nB2\nB1\nc\nd\nE\nf\nG\n");
        assert_eq!(m.regions(&text)[0].state, State::Both { first: 2, smart: false });
        assert_eq!(m.with_markers(&m.auto_merged(), &[true, false, false]), "a\n<<<<<<< Current\nB2\n=======\nB1\n>>>>>>> Incoming\nc\nd\nE\nf\nG\n");
    }

    #[test]
    fn combines_changes_that_touch() {
        // Incoming inserts after "a"; current changes "b": they touch, so one range.
        let m = Merge::new("a\nb\nc", "a\nnew\nb\nc", "a\nB\nc");
        assert_eq!(m.ranges.len(), 1);
        assert!(m.is_conflicting(0));
        assert!(m.can_be_combined(0));
        assert_eq!(m.candidate(0, State::Both { first: 1, smart: true }).unwrap(), vec!["new", "B"]);
        // Both insert at the same place: the order matters.
        let m = Merge::new("a\nc", "a\nx\nc", "a\ny\nc");
        assert!(m.can_be_combined(0) && m.is_order_relevant(0));
        assert_eq!(m.candidate(0, State::Both { first: 2, smart: true }).unwrap(), vec!["y", "x"]);
        // Both change the same line: no combination.
        let m = Merge::new(BASE, IN1, IN2);
        assert!(!m.can_be_combined(0));
        // The same change on both sides isn't a conflict.
        let m = Merge::new("a\nb", "a\nB", "a\nB");
        assert!(!m.is_conflicting(0));
        assert_eq!(m.auto_merged(), "a\nB");
    }

    #[test]
    fn state_toggles() {
        assert_eq!(State::Base.with(1, true), State::Input1);
        assert_eq!(State::Input2.with(1, true), State::Both { first: 2, smart: false });
        assert_eq!(State::Both { first: 1, smart: true }.with(1, false), State::Input2);
        assert_eq!(State::Input1.with(1, false), State::Base);
    }
}
