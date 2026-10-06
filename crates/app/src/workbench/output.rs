//! The Output panel's channels: "Language Servers" (the servers' logs) and one
//! per extension or channel an extension writes to, picked from the dropdown in the panel's
//! title bar. The view follows the end of the channel unless scrolled back.

use render::{Canvas, Rect, TextStyle};

use super::preferences::PopupAction;
use super::{Hit, PopupItem, Workbench};
use crate::icons;

pub(super) const LANGUAGE_SERVERS: &str = "Language Servers";
const LINE_H: f32 = 18.0;
/// Lines kept per channel.
const MAX_LINES: usize = 10_000;

#[derive(Default)]
pub(super) struct Output {
    /// (name, lines, the last line while it has no newline yet).
    channels: Vec<(String, Vec<String>, String)>,
    /// The channel shown (None: "Language Servers").
    pub active: Option<String>,
    /// Lines scrolled back from the end.
    scroll: f32,
}

impl Output {
    fn channel(&mut self, name: &str) -> &mut (String, Vec<String>, String) {
        let i = match self.channels.iter().position(|c| c.0 == name) {
            Some(i) => i,
            None => {
                self.channels.push((name.to_string(), Vec::new(), String::new()));
                self.channels.sort_by_key(|c| c.0.to_lowercase());
                self.channels.iter().position(|c| c.0 == name).unwrap()
            }
        };
        &mut self.channels[i]
    }

    /// Appends text (which may hold several lines, or part of one) to a channel, making it.
    pub fn append(&mut self, name: &str, text: &str) {
        let shown = self.active.as_deref() == Some(name);
        let (_, lines, partial) = self.channel(name);
        let before = lines.len();
        partial.push_str(text);
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            lines.push(line.trim_end_matches(['\n', '\r']).to_string());
        }
        if lines.len() > MAX_LINES {
            lines.drain(..lines.len() - MAX_LINES);
        }
        let added = lines.len().saturating_sub(before);
        // Scrolled back: stay on the same lines.
        if shown && self.scroll > 0.0 {
            self.scroll += added as f32;
        }
    }

    pub fn clear(&mut self, name: &str) {
        let (_, lines, partial) = self.channel(name);
        lines.clear();
        partial.clear();
    }

    pub fn names(&self) -> Vec<String> {
        self.channels.iter().map(|c| c.0.clone()).collect()
    }

    pub(super) fn lines(&self, name: &str) -> Vec<String> {
        let Some((_, lines, partial)) = self.channels.iter().find(|c| c.0 == name) else { return Vec::new() };
        let mut out = lines.clone();
        if !partial.is_empty() {
            out.push(partial.clone());
        }
        out
    }
}

impl Workbench {
    /// Shows a channel in the Output panel.
    pub(super) fn show_output_channel(&mut self, name: &str, focus: bool) {
        self.output.active = (name != LANGUAGE_SERVERS).then(|| name.to_string());
        self.output.scroll = 0.0;
        self.panel_visible = true;
        self.panel_tab = 1;
        if focus {
            self.focus = super::Focus::Editor;
        }
    }

    fn output_lines(&self) -> Vec<String> {
        match &self.output.active {
            Some(name) => self.output.lines(name),
            None => self.lsp.output.clone(),
        }
    }

    /// The Output panel: the chosen channel, newest at the bottom.
    pub(super) fn draw_output(&mut self, c: &mut Canvas, body: Rect) {
        let style = TextStyle::mono(12.0, LINE_H, self.color("foreground"));
        let lines = self.output_lines();
        let n = ((body.h - 8.0) / LINE_H).floor().max(0.0) as usize;
        let back = self.output.scroll.clamp(0.0, lines.len().saturating_sub(n) as f32) as usize;
        self.output.scroll = back as f32;
        let end = lines.len() - back;
        let start = end.saturating_sub(n);
        c.push_clip(body);
        for (i, line) in lines[start..end].iter().enumerate() {
            c.text(body.x + 20.0, body.y + 4.0 + i as f32 * LINE_H, line, &style);
        }
        c.pop_clip();
    }

    pub(super) fn scroll_output(&mut self, dy: f32) {
        self.output.scroll = (self.output.scroll + dy / LINE_H).max(0.0);
    }

    /// The channel chip in the panel's title bar, ending at `right`; it opens a menu of channels.
    pub(super) fn draw_output_channel_picker(&mut self, c: &mut Canvas, header: Rect, right: f32) {
        let name = self.output.active.clone().unwrap_or_else(|| LANGUAGE_SERVERS.to_string());
        let fg = self.color("foreground");
        let st = TextStyle::ui(12.0, fg);
        let w = (c.measure(&name, &st) + 52.0).clamp(110.0, 260.0);
        let r = Rect::new(right - w, header.y + 6.0, w, 24.0);
        let bg = if self.hovered(Hit::OutputChannels) { "toolbar.hoverBackground" } else { "input.background" };
        c.fill_rounded(r, self.color(bg), 12.0);
        let dim = self.color("descriptionForeground");
        c.icon(&icons::LIST_SELECTION, r.x + 9.0, r.y + 5.0, 14.0, dim);
        c.text_fit(Rect::new(r.x + 28.0, r.y, r.w - 50.0, r.h), &name, &st);
        c.icon(&icons::CHEVRON_DOWN, r.right() - 21.0, r.y + 4.0, 16.0, dim);
        self.hits.push((r, Hit::OutputChannels));
    }

    pub(super) fn pick_output_channel(&mut self, x: f32, y: f32) {
        let active = self.output.active.clone().unwrap_or_else(|| LANGUAGE_SERVERS.to_string());
        let mut names = vec![LANGUAGE_SERVERS.to_string()];
        names.extend(self.output.names());
        let entries = names
            .into_iter()
            .map(|n| (PopupItem::Item { label: n.clone(), enabled: true, checked: Some(n == active) }, PopupAction::OutputChannel(n)))
            .collect();
        // Under the chip, when it was clicked.
        let chip = self.hits.iter().find(|(_, h)| *h == Hit::OutputChannels).map(|(r, _)| *r);
        let (x, y) = chip.map_or((x, y), |r| (r.x, r.bottom() + 4.0));
        self.show_popup(entries, x, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_partial_lines() {
        let mut o = Output::default();
        o.append("Ext", "one\ntw");
        assert_eq!(o.lines("Ext"), ["one", "tw"]);
        o.append("Ext", "o\r\n");
        assert_eq!(o.lines("Ext"), ["one", "two"]);
        o.append("A", "x\n");
        assert_eq!(o.names(), ["A", "Ext"]);
        o.clear("Ext");
        assert!(o.lines("Ext").is_empty());
    }
}
