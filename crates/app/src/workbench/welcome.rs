//! The Welcome page: a tab (`EditorState::welcome`) with ways to start (new file, open or clone
//! a folder, recent folders), the built-in themes to pick from, the Assistant's agent and where
//! to learn the rest. It opens at startup when nothing else is (`workbench.startupEditor`) and
//! from Help → Welcome.

use std::path::PathBuf;

use render::{Canvas, Color, Rect, TextStyle};
use serde_json::Value;

use super::{Focus, Hit, Workbench, SMALL, UI};
use crate::commands::Command;
use crate::editor::EditorState;
use crate::icons;

/// What a click on the page does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WelcomeHit {
    Body,
    Run(Command),
    /// A recent folder (its index in `Welcome::recent`).
    Recent(usize),
    /// A built-in theme (its index in `Welcome::themes`).
    Theme(usize),
    ShowOnStartup,
}

/// A theme's look for its tile: (name, background, foreground, accent).
type ThemeTile = (String, Color, Color, Color);

#[derive(Default)]
pub(super) struct Welcome {
    /// Pixels scrolled, and the page's height in the last frame.
    scroll: f32,
    content_h: f32,
    body: Rect,
    /// The recent folders shown (read when the page opens).
    recent: Vec<PathBuf>,
    /// The built-in themes' colors (loaded the first time the page draws).
    themes: Vec<ThemeTile>,
}

const MAX_W: f32 = 860.0;
const RECENT: usize = 7;

/// "~/Repo/app" for a path under the home folder.
fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

impl Workbench {
    /// Help → Welcome: opens (or shows) the page.
    pub(super) fn open_welcome(&mut self) {
        self.welcome.recent = super::session::recent_folders();
        self.welcome.scroll = 0.0;
        let existing = self.groups.iter().enumerate().find_map(|(g, gr)| gr.tabs.iter().position(|t| t.welcome).map(|i| (g, i)));
        match existing {
            Some((g, i)) => {
                self.active_group = g;
                self.groups[g].active = i;
            }
            None => {
                let doc = self.add_doc(crate::editor::Doc::virtual_named("Welcome"));
                let g = &mut self.groups[self.active_group];
                let at = if g.tabs.is_empty() { 0 } else { g.active + 1 };
                let mut ed = EditorState::new(doc);
                ed.welcome = true;
                g.tabs.insert(at, ed);
                g.active = at;
            }
        }
        self.focus = Focus::Editor;
    }

    /// At startup: the page, if nothing else opened and the setting asks for it.
    pub(super) fn welcome_at_startup(&mut self) {
        if self.settings.string("workbench.startupEditor") == "welcomePage" && self.groups.iter().all(|g| g.tabs.is_empty()) {
            self.open_welcome();
        }
    }

    pub(super) fn welcome_click(&mut self, hit: WelcomeHit) {
        match hit {
            WelcomeHit::Body => {}
            WelcomeHit::Run(cmd) => self.run(cmd),
            WelcomeHit::Recent(i) => {
                if let Some(path) = self.welcome.recent.get(i).cloned() {
                    self.open_folder_by_user(&path);
                }
            }
            WelcomeHit::Theme(i) => {
                if let Some((name, ..)) = self.welcome.themes.get(i).cloned() {
                    self.update_setting(settings::Scope::User, "workbench.colorTheme", Some(Value::String(name)));
                    self.apply_settings();
                }
            }
            WelcomeHit::ShowOnStartup => {
                let on = self.settings.string("workbench.startupEditor") == "welcomePage";
                let value = if on { "none" } else { "welcomePage" };
                self.update_setting(settings::Scope::User, "workbench.startupEditor", Some(Value::String(value.into())));
                self.apply_settings();
            }
        }
    }

    pub(super) fn welcome_scroll(&mut self, dy: f32) {
        let w = &mut self.welcome;
        let max = (w.content_h - w.body.h).max(0.0);
        w.scroll = (w.scroll - dy).clamp(0.0, max);
    }

    /// The built-in themes' colors, for their tiles.
    fn welcome_themes(&mut self) {
        if !self.welcome.themes.is_empty() {
            return;
        }
        for info in theme::builtin_themes() {
            let Ok(t) = theme::Theme::load(&info) else { continue };
            self.welcome.themes.push((info.name.clone(), t.color("editor.background"), t.color("editor.foreground"), t.color("focusBorder")));
        }
    }

    pub(super) fn draw_welcome(&mut self, c: &mut Canvas, r: Rect) {
        self.welcome_themes();
        c.fill(r, self.color("editor.background"));
        self.welcome.body = r;
        self.hits.push((r, Hit::Welcome(WelcomeHit::Body)));
        let fg = self.color("foreground");
        let dim = self.color("descriptionForeground");
        let w = (r.w - 80.0).clamp(240.0, MAX_W);
        let x0 = r.x + ((r.w - w) / 2.0).max(16.0);
        let top = r.y + 44.0 - self.welcome.scroll;
        let mut y = top;
        c.text(x0, y, "Orbvane", &TextStyle::ui(30.0, fg).weight(600));
        y += 44.0;
        c.text(x0, y, "A native code editor for Mac", &TextStyle::ui(15.0, dim));
        y += 44.0;

        // Two columns when there's room.
        let two = w >= 620.0;
        let col_w = if two { (w - 48.0) / 2.0 } else { w };
        let left_end = self.welcome_start(c, x0, y, col_w, fg, dim);
        let (rx, ry) = if two { (x0 + col_w + 48.0, y) } else { (x0, left_end + 20.0) };
        let right_end = self.welcome_setup(c, rx, ry, col_w, fg, dim);
        // Show on startup: under the first column (the shorter one), else at the end.
        let mut end = if two { left_end + 28.0 } else { right_end + 28.0 };

        let on = self.settings.string("workbench.startupEditor") == "welcomePage";
        let label = "Show the Welcome page on startup";
        let st = TextStyle::ui(UI, dim);
        let row = Rect::new(x0, end, c.measure(label, &st) + 26.0, 20.0);
        let b = Rect::new(row.x, row.y + 3.0, 14.0, 14.0);
        c.bordered(b, self.color("checkbox.background"), self.color("checkbox.border"), 1.0, 3.0);
        if on {
            c.icon_in(&icons::CHECK, b, 12.0, self.color("checkbox.foreground"));
        }
        c.text(row.x + 22.0, row.y + 1.0, label, &st);
        self.hits.push((row.intersect(&r), Hit::Welcome(WelcomeHit::ShowOnStartup)));
        end = (end + 20.0).max(right_end) + 48.0;
        self.welcome.content_h = end - top;
    }

    /// A section's heading; returns the y below it.
    fn welcome_heading(c: &mut Canvas, x: f32, y: f32, text: &str, fg: Color) -> f32 {
        c.text(x, y, text, &TextStyle::ui(14.0, fg).weight(600));
        y + 28.0
    }

    /// A link with an icon; returns the y below it.
    fn welcome_link(&mut self, c: &mut Canvas, x: f32, y: f32, w: f32, icon: &render::Icon, label: &str, hit: WelcomeHit) -> f32 {
        let hovered = self.hovered(Hit::Welcome(hit));
        let color = if hovered { self.color_or("textLink.activeForeground", "textLink.foreground") } else { self.color("textLink.foreground") };
        let st = TextStyle::ui(UI, color);
        let lw = c.measure(label, &st).min(w - 24.0);
        c.icon(icon, x, y + 3.0, 15.0, color);
        c.text_fit(Rect::new(x + 24.0, y, lw + 2.0, 22.0), label, &st);
        let rect = Rect::new(x, y, lw + 26.0, 22.0);
        self.hits.push((rect.intersect(&self.welcome.body), Hit::Welcome(hit)));
        y + 28.0
    }

    /// Start: new file, open, clone, and the recent folders.
    fn welcome_start(&mut self, c: &mut Canvas, x: f32, y: f32, w: f32, fg: Color, dim: Color) -> f32 {
        let mut y = Self::welcome_heading(c, x, y, "Start", fg);
        y = self.welcome_link(c, x, y, w, &icons::NEW_FILE, "New File", WelcomeHit::Run(Command::NewFile));
        y = self.welcome_link(c, x, y, w, &icons::FOLDER, "Open Folder…", WelcomeHit::Run(Command::OpenFolder));
        y = self.welcome_link(c, x, y, w, &icons::SOURCE_CONTROL, "Clone Git Repository…", WelcomeHit::Run(Command::GitClone));
        y += 18.0;
        y = Self::welcome_heading(c, x, y, "Recent", fg);
        let recent = self.welcome.recent.clone();
        if recent.is_empty() {
            c.text(x, y, "Folders you open show here.", &TextStyle::ui(UI, dim));
            return y + 24.0;
        }
        let small = TextStyle::ui(UI, dim);
        for (i, path) in recent.iter().take(RECENT).enumerate() {
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let hovered = self.hovered(Hit::Welcome(WelcomeHit::Recent(i)));
            let color = if hovered { self.color_or("textLink.activeForeground", "textLink.foreground") } else { self.color("textLink.foreground") };
            let st = TextStyle::ui(UI, color);
            let nw = c.measure(&name, &st).min(w * 0.5);
            c.text_fit(Rect::new(x, y, nw + 2.0, 22.0), &name, &st);
            let parent = path.parent().map(|p| tilde(p)).unwrap_or_default();
            c.text_fit(Rect::new(x + nw + 12.0, y, (w - nw - 12.0).max(0.0), 22.0), &parent, &small);
            self.hits.push((Rect::new(x, y, nw + 2.0, 22.0).intersect(&self.welcome.body), Hit::Welcome(WelcomeHit::Recent(i))));
            y += 26.0;
        }
        if recent.len() > RECENT {
            y = self.welcome_link(c, x, y, w, &icons::HISTORY, "More…", WelcomeHit::Run(Command::OpenRecent));
        }
        y
    }

    /// Set up: theme, the Assistant's agent, and where to learn more.
    fn welcome_setup(&mut self, c: &mut Canvas, x: f32, y: f32, w: f32, fg: Color, dim: Color) -> f32 {
        let mut y = Self::welcome_heading(c, x, y, "Theme", fg);
        let current = self.settings.string("workbench.colorTheme");
        let themes = self.welcome.themes.clone();
        let tile_w = ((w - 2.0 * 12.0) / 3.0).clamp(90.0, 140.0);
        let per_row = ((w + 12.0) / (tile_w + 12.0) + 0.01).floor().max(1.0) as usize;
        for (i, (name, bg, tfg, accent)) in themes.iter().enumerate() {
            let tx = x + (i % per_row) as f32 * (tile_w + 12.0);
            let ty = y + (i / per_row) as f32 * 96.0;
            let tile = Rect::new(tx, ty, tile_w, 64.0);
            let chosen = *name == current;
            let hovered = self.hovered(Hit::Welcome(WelcomeHit::Theme(i)));
            let border = if chosen { self.color("focusBorder") } else if hovered { self.color("contrastActiveBorder") } else { self.color("widget.border") };
            c.bordered(tile, *bg, border, if chosen { 2.0 } else { 1.0 }, 8.0);
            // A few lines of "code" in the theme's colors.
            for (k, frac) in [0.55f32, 0.75, 0.4].iter().enumerate() {
                let line = Rect::new(tile.x + 12.0, tile.y + 14.0 + k as f32 * 13.0, (tile.w - 24.0) * frac, 5.0);
                c.fill_rounded(line, if k == 1 { *accent } else { tfg.with_alpha(0.6) }, 2.5);
            }
            c.text_fit(Rect::new(tx, ty + 68.0, tile_w, 20.0), name, &TextStyle::ui(SMALL, if chosen { fg } else { dim }));
            self.hits.push((Rect::new(tx, ty, tile_w, 88.0).intersect(&self.welcome.body), Hit::Welcome(WelcomeHit::Theme(i))));
        }
        y += themes.len().div_ceil(per_row) as f32 * 96.0;
        y = self.welcome_link(c, x, y, w, &icons::COLOR_MODE, "More Themes…", WelcomeHit::Run(Command::SelectTheme));
        y += 18.0;

        y = Self::welcome_heading(c, x, y, "Assistant", fg);
        let about = match self.agent_choice() {
            crate::agents::Choice::None => "Chat with Claude Code, Codex or another coding agent while you work. It reads and edits this folder, and asks before it changes anything.".to_string(),
            choice => format!("New chats talk to {}. Several chats can run at once, each kept with the folder.", choice.label()),
        };
        let st = TextStyle::ui(UI, dim);
        for line in super::intel::wrap(c, &about, &st, w) {
            c.text(x, y, &line, &st);
            y += 18.0;
        }
        y += 8.0;
        y = self.welcome_link(c, x, y, w, &icons::ACCOUNT, "Open the Assistant", WelcomeHit::Run(Command::AssistantFocus));
        y = self.welcome_link(c, x, y, w, &icons::SETTINGS, "Choose the Agent…", WelcomeHit::Run(Command::AssistantSelectAgent));
        y += 18.0;

        y = Self::welcome_heading(c, x, y, "Learn", fg);
        for (icon, label, cmd) in [
            (&icons::SEARCH, "Show All Commands", Command::CommandPalette),
            (&icons::GO_TO_FILE, "Keyboard Shortcuts", Command::OpenKeybindings),
            (&icons::GEAR, "Settings", Command::OpenSettings),
        ] {
            let after = self.welcome_link(c, x, y, w, icon, label, WelcomeHit::Run(cmd));
            if let Some(caps) = crate::keymap::keycaps(cmd) {
                let lw = c.measure(label, &TextStyle::ui(UI, dim)) + 36.0;
                if lw + 80.0 < w {
                    self.keycaps(c, x + lw, y + 1.0, &caps, dim);
                }
            }
            y = after;
        }
        y
    }
}
