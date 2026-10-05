//! The Explorer's sections, like the standard split view: the folder tree and extensions' views, each a header plus (when open) a body. The first open section fills the space the others
//! leave; the others keep the height they were dragged to (the sash above their header).

use render::Rect;

use super::{Workbench, ROW_H};

pub(super) const MIN_BODY: f32 = ROW_H * 2.0;
/// The built-in sections; extensions' Explorer views follow.
pub(super) const BUILTIN: usize = 1;

/// Where each section was laid out: header and body (empty when closed).
#[derive(Clone, Default)]
pub(super) struct SectionRects {
    pub head: Vec<Rect>,
    pub body: Vec<Rect>,
}

impl Workbench {
    /// The extension views shown in the Explorer (indices into `ext_views.list`).
    pub(super) fn explorer_ext_views(&self) -> Vec<usize> {
        self.ext_views_in("explorer")
    }

    fn section_count(&self) -> usize {
        BUILTIN + self.explorer_ext_views().len()
    }

    fn ext_section_state(&self, i: usize) -> Option<&super::ext_views::TreeState> {
        let vi = *self.explorer_ext_views().get(i - BUILTIN)?;
        self.ext_views.state.get(&self.ext_views.list[vi].id)
    }

    fn section_open(&self, i: usize) -> bool {
        match i {
            0 => self.explorer_open,
            _ => self.ext_section_state(i).is_none_or(|s| s.open),
        }
    }

    fn section_height(&self, i: usize) -> Option<f32> {
        match i {
            0 => None,
            _ => self.ext_section_state(i).and_then(|s| s.height),
        }
    }

    /// Lays out the sections in `r`. Closed sections are just headers; while all are
    /// closed, the headers after the first sit at the bottom.
    pub(super) fn layout_sections(&self, r: Rect) -> SectionRects {
        let n = self.section_count();
        let open: Vec<bool> = (0..n).map(|i| self.section_open(i)).collect();
        let heights: Vec<Option<f32>> = (0..n).map(|i| self.section_height(i)).collect();
        let (head, body) = split(r, &open, &heights);
        SectionRects { head, body }
    }

    /// Whether section `i` has a sash above its header (an open section above it).
    pub(super) fn has_sash(&self, i: usize) -> bool {
        i > 0 && self.section_open(i) && (0..i).any(|j| self.section_open(j))
    }

    /// Dragging the sash above section `i`'s header to `y`.
    pub(super) fn drag_section_sash(&mut self, i: usize, y: f32) {
        let Some(body) = self.sections.body.get(i).copied() else { return };
        let h = Some((body.bottom() - y - ROW_H).max(MIN_BODY));
        match i {
            0 => {}
            _ => {
                if let Some(&vi) = self.explorer_ext_views().get(i - BUILTIN) {
                    self.ext_tree_state(vi).height = h;
                }
            }
        }
    }
}

/// Splits `r` into sections like the standard split views: each a header, plus a body when open.
/// The first open section fills the space the others leave; the others keep `heights` (the
/// height they were dragged to; None: an even share). Returns the headers and bodies (empty
/// when closed; while all are closed, the first body is the empty space under its header).
pub(super) fn split(r: Rect, open: &[bool], heights: &[Option<f32>]) -> (Vec<Rect>, Vec<Rect>) {
    let n = open.len();
    let avail = (r.h - ROW_H * n as f32).max(0.0);
    let open_ix: Vec<usize> = (0..n).filter(|&i| open[i]).collect();
    let mut hs = vec![0.0f32; n];
    match open_ix.split_first() {
        None => hs[0] = avail,
        Some((&filler, rest)) => {
            let share = avail / open_ix.len() as f32;
            let mut fixed: Vec<f32> = rest.iter().map(|&i| heights[i].unwrap_or(share).max(MIN_BODY)).collect();
            // Leave the filler at least MIN_BODY.
            let total: f32 = fixed.iter().sum();
            let room = (avail - MIN_BODY).max(0.0);
            if total > room && total > 0.0 {
                for h in &mut fixed {
                    *h *= room / total;
                }
            }
            for (&i, h) in rest.iter().zip(&fixed) {
                hs[i] = *h;
            }
            hs[filler] = (avail - fixed.iter().sum::<f32>()).max(0.0);
        }
    }
    let (mut heads, mut bodies) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut rest = r;
    for h in hs {
        let (head, after) = rest.cut_top(ROW_H);
        let (body, after) = after.cut_top(h);
        heads.push(head);
        bodies.push(body);
        rest = after;
    }
    (heads, bodies)
}
