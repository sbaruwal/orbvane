//! The secondary sidebar on the right: the assistant (`assistant.rs`), the active file's Outline
//! and its Timeline, one at a time behind a segmented control. Toggled from the toolbar or with ⌥⌘B.

use render::{Canvas, Rect, TextStyle};

use super::{Hit, Workbench};
use crate::icons;

pub(super) const AUX_HEADER_H: f32 = 34.0;
pub(super) const AUX_MIN_W: f32 = 240.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub(super) enum AuxTab {
    #[default]
    Assistant,
    Outline,
    Timeline,
}

impl AuxTab {
    pub(super) const ALL: [AuxTab; 3] = [AuxTab::Assistant, AuxTab::Outline, AuxTab::Timeline];

    fn label(self) -> &'static str {
        match self {
            AuxTab::Assistant => "Assistant",
            AuxTab::Outline => "Outline",
            AuxTab::Timeline => "Timeline",
        }
    }
}

pub(super) struct AuxBar {
    pub visible: bool,
    pub width: f32,
    pub tab: AuxTab,
}

impl Default for AuxBar {
    fn default() -> Self {
        Self { visible: false, width: 360.0, tab: AuxTab::Assistant }
    }
}

impl Workbench {
    pub(super) fn toggle_aux_bar(&mut self) {
        self.aux.visible = !self.aux.visible;
    }

    /// Shows the secondary sidebar on `tab`.
    pub(super) fn show_aux(&mut self, tab: AuxTab) {
        self.aux.visible = true;
        self.aux.tab = tab;
    }

    /// Whether `tab` is showing.
    pub(super) fn aux_showing(&self, tab: AuxTab) -> bool {
        self.aux.visible && self.aux.tab == tab && self.zen.is_none()
    }

    /// The sash on its left edge was dragged to `x`.
    pub(super) fn drag_aux_sash(&mut self, x: f32) {
        let right = self.main_rect.right();
        self.aux.width = (right - x).clamp(AUX_MIN_W, (self.main_rect.w - 420.0).max(AUX_MIN_W));
    }

    pub(super) fn draw_aux_bar(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, self.color("sideBar.background"));
        c.fill(Rect::new(r.x, r.y, 1.0, r.h), self.color("sideBar.border"));
        self.hits.push((r, Hit::AuxBody));
        c.push_clip(r);
        let (header, body) = r.cut_top(AUX_HEADER_H);
        let mut x = header.x + 8.0;
        for (i, tab) in AuxTab::ALL.into_iter().enumerate() {
            let active = self.aux.tab == tab;
            let hovered = self.hovered(Hit::AuxTab(i as u8));
            let color = self.color(if active || hovered { "panelTitle.activeForeground" } else { "panelTitle.inactiveForeground" });
            let style = TextStyle::ui(12.0, color).weight(if active { 600 } else { 400 });
            let w = c.measure(tab.label(), &style) + 20.0;
            let chip = Rect::new(x, header.y + 6.0, w, header.h - 12.0);
            if active {
                c.fill_rounded(chip, self.color_or("activityBarTop.activeBackground", "list.inactiveSelectionBackground"), 6.0);
            } else if hovered {
                c.fill_rounded(chip, self.color("toolbar.hoverBackground"), 6.0);
            }
            c.text_in(Rect::new(chip.x + 10.0, header.y, w, header.h), tab.label(), &style);
            self.hits.push((chip, Hit::AuxTab(i as u8)));
            self.a11y_name(Hit::AuxTab(i as u8), super::a11y::Role::Tab, tab.label(), active);
            x += w + 4.0;
        }
        let fg = self.color("icon.foreground");
        let close = Rect::new(header.right() - 32.0, header.y + 6.0, 24.0, 22.0);
        self.icon_button(c, close, &icons::CLOSE, Hit::AuxClose, fg);
        if self.aux.tab == AuxTab::Outline {
            // Collapse All and the "..." menu (Follow Cursor, Sort By).
            let more = Rect::new(close.x - 26.0, close.y, 24.0, 22.0);
            let collapse = Rect::new(more.x - 26.0, close.y, 24.0, 22.0);
            self.icon_button(c, collapse, &icons::COLLAPSE_ALL, Hit::OutlineAction(0), fg);
            self.icon_button(c, more, &icons::ELLIPSIS, Hit::OutlineAction(1), fg);
            self.outline_menu_at = (more.x, more.bottom());
        }
        c.fill(Rect::new(r.x, header.bottom() - 1.0, r.w, 1.0), self.color("sideBarSectionHeader.border"));
        match self.aux.tab {
            AuxTab::Outline => self.draw_outline(c, body),
            AuxTab::Assistant => self.draw_assistant(c, body),
            AuxTab::Timeline => self.draw_timeline(c, body),
        }
        c.pop_clip();
    }
}
