//! Notification toasts, the bottom-right messages: an icon for the severity, the message,
//! "Source: ..." and buttons. Extensions show them (`window/showMessage`, and
//! `window/showMessageRequest`, which waits for the button clicked). Toasts without buttons go
//! away after a while, except errors.

use std::time::{Duration, Instant};

use render::{Canvas, Rect, TextStyle};
use serde_json::Value;

use super::{Hit, Workbench};
use crate::icons;

const WIDTH: f32 = 450.0;
const LINE_H: f32 = 20.0;
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SHOWN: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// LSP's `MessageType` (1 error, 2 warning, 3 info, 4 log).
    pub fn from_lsp(n: u64) -> Self {
        match n {
            1 => Severity::Error,
            2 => Severity::Warning,
            _ => Severity::Info,
        }
    }
}

pub(super) struct Toast {
    id: u64,
    severity: Severity,
    message: String,
    /// "Source: <extension>".
    source: String,
    actions: Vec<String>,
    /// The extension request waiting for the answer: (extension id, request id).
    reply: Option<(String, Value)>,
    /// The editor's own toasts: called with the button clicked (None: closed) and `data`.
    handler: Option<ToastHandler>,
    data: String,
    at: Instant,
}

/// Called with the button clicked (None: closed) and the toast's data.
pub(super) type ToastHandler = fn(&mut Workbench, Option<&str>, &str);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToastHit {
    Body,
    Close,
    Action(usize),
}

#[derive(Default)]
pub(super) struct Toasts {
    list: Vec<Toast>,
    next_id: u64,
}

impl Toast {
    fn expires(&self) -> Option<Instant> {
        (self.actions.is_empty() && self.severity != Severity::Error).then(|| self.at + TIMEOUT)
    }
}

impl Workbench {
    pub(super) fn notify(&mut self, severity: Severity, message: &str, source: &str, actions: Vec<String>, reply: Option<(String, Value)>) {
        let t = &mut self.toasts;
        t.next_id += 1;
        // The same message again replaces the old one.
        if let Some(i) = t.list.iter().position(|x| x.message == message && x.source == source && x.reply.is_none() && reply.is_none()) {
            t.list.remove(i);
        }
        t.list.push(Toast { id: t.next_id, severity, message: message.to_string(), source: source.to_string(), actions, reply, handler: None, data: String::new(), at: Instant::now() });
    }

    /// A toast of the editor's own whose buttons call `handler` (with the button's title, or None
    /// when it's closed, and `data`). It replaces an earlier toast with the same handler and data.
    pub(super) fn notify_with(&mut self, severity: Severity, message: &str, actions: Vec<String>, handler: ToastHandler, data: &str) {
        let t = &mut self.toasts;
        t.next_id += 1;
        t.list.retain(|x| !(x.handler.is_some_and(|h| std::ptr::fn_addr_eq(h, handler)) && x.data == data));
        let (message, data) = (message.to_string(), data.to_string());
        t.list.push(Toast { id: t.next_id, severity, message, source: String::new(), actions, reply: None, handler: Some(handler), data, at: Instant::now() });
    }

    /// Closes a toast; `action` is the button clicked (None: closed).
    pub(super) fn close_toast(&mut self, id: u64, action: Option<usize>) {
        let Some(i) = self.toasts.list.iter().position(|t| t.id == id) else { return };
        let toast = self.toasts.list.remove(i);
        let title = action.and_then(|a| toast.actions.get(a));
        if let Some((ext, request)) = toast.reply {
            let answer = title.map_or(Value::Null, |title| serde_json::json!({ "title": title }));
            self.ext_respond(&ext, request, Ok(answer));
        }
        if let Some(handler) = toast.handler {
            handler(self, title.map(String::as_str), &toast.data);
        }
    }

    pub(super) fn toast_click(&mut self, id: u64, hit: ToastHit) {
        match hit {
            ToastHit::Body => {}
            ToastHit::Close => self.close_toast(id, None),
            ToastHit::Action(a) => self.close_toast(id, Some(a)),
        }
    }

    /// Escape closes the newest toast. Returns whether there was one.
    pub(super) fn close_newest_toast(&mut self) -> bool {
        let Some(id) = self.toasts.list.last().map(|t| t.id) else { return false };
        self.close_toast(id, None);
        true
    }

    /// Drops timed out toasts (not while the pointer is on one).
    fn expire_toasts(&mut self) {
        let hovering = matches!(self.hover_hit, Some(Hit::Toast(..)));
        let now = Instant::now();
        let expired: Vec<u64> = self.toasts.list.iter().filter(|t| !hovering && t.expires().is_some_and(|e| e <= now)).map(|t| t.id).collect();
        for id in expired {
            self.close_toast(id, None);
        }
    }

    /// (id, message, buttons) of the toasts, oldest first.
    #[cfg(test)]
    pub(super) fn toast_list(&self) -> Vec<(u64, String, Vec<String>)> {
        self.toasts.list.iter().map(|t| (t.id, t.message.clone(), t.actions.clone())).collect()
    }

    pub(super) fn toasts_deadline(&self) -> Option<Instant> {
        self.toasts.list.iter().filter_map(Toast::expires).min()
    }

    /// The toasts, stacked up from the bottom right corner of `area` (newest at the bottom).
    pub(super) fn draw_toasts(&mut self, c: &mut Canvas, area: Rect) {
        self.expire_toasts();
        if self.toasts.list.is_empty() {
            return;
        }
        c.push_layer();
        let fg = self.color("notifications.foreground");
        let style = TextStyle::ui(13.0, fg);
        let source_style = TextStyle::ui(12.0, self.color("descriptionForeground"));
        let w = WIDTH.min(area.w - 16.0);
        let mut bottom = area.bottom() - 10.0;
        let shown: Vec<usize> = (0..self.toasts.list.len()).rev().take(MAX_SHOWN).collect();
        for i in shown {
            let t = &self.toasts.list[i];
            let (id, severity, actions) = (t.id, t.severity, t.actions.clone());
            let lines = super::intel::wrap(c, &t.message, &style, w - 16.0 - 26.0 - 30.0);
            let source = (!t.source.is_empty()).then(|| format!("Source: {}", t.source));
            let footer_h = if actions.is_empty() && source.is_none() { 0.0 } else { 34.0 };
            let h = 10.0 + lines.len() as f32 * LINE_H + footer_h + 6.0;
            let r = Rect::new(area.right() - 10.0 - w, bottom - h, w, h);
            bottom = r.y - 8.0;
            c.shadow(r, super::controls::CARD_RADIUS, self.color("widget.shadow"));
            c.bordered(r, self.color("notifications.background"), self.color("notificationToast.border"), 1.0, super::controls::CARD_RADIUS);
            self.hits.push((r, Hit::Toast(id, ToastHit::Body)));
            let (icon, color) = match severity {
                Severity::Error => (&icons::ERROR, "notificationsErrorIcon.foreground"),
                Severity::Warning => (&icons::WARNING, "notificationsWarningIcon.foreground"),
                Severity::Info => (&icons::INFO, "notificationsInfoIcon.foreground"),
            };
            c.icon(icon, r.x + 12.0, r.y + 12.0, 16.0, self.color(color));
            for (n, line) in lines.iter().enumerate() {
                c.text_in(Rect::new(r.x + 38.0, r.y + 8.0 + n as f32 * LINE_H, r.w - 38.0 - 34.0, LINE_H), line, &style);
            }
            let close = Rect::new(r.right() - 30.0, r.y + 7.0, 22.0, 22.0);
            self.icon_button(c, close, &icons::CLOSE, Hit::Toast(id, ToastHit::Close), self.color("icon.foreground"));
            let fy = r.bottom() - 6.0 - 28.0;
            if let Some(source) = &source {
                c.text_in(Rect::new(r.x + 38.0, fy, r.w * 0.5, 26.0), source, &source_style);
            }
            // Buttons, right-aligned; the first is the primary one.
            let mut x = r.right() - 10.0;
            for (a, label) in actions.iter().enumerate().rev() {
                let primary = a == 0;
                let st = TextStyle::ui(13.0, self.color(if primary { "button.foreground" } else { "button.secondaryForeground" }));
                let bw = c.measure(label, &st) + 22.0;
                x -= bw;
                let b = Rect::new(x, fy + 2.0, bw, 24.0);
                let hit = Hit::Toast(id, ToastHit::Action(a));
                let bg = match (primary, self.hovered(hit)) {
                    (true, false) => "button.background",
                    (true, true) => "button.hoverBackground",
                    (false, false) => "button.secondaryBackground",
                    (false, true) => "button.secondaryHoverBackground",
                };
                c.bordered(b, self.color(bg), self.color_or("button.border", "contrastBorder"), 1.0, super::controls::FIELD_RADIUS);
                c.text_in(Rect::new(b.x + 11.0, b.y, bw - 22.0, b.h), label, &st);
                self.hits.push((b, hit));
                x -= 8.0;
            }
        }
    }
}
