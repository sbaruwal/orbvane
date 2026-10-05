//! The Extensions view (⇧⌘X): the installed extensions with a search box, a gear menu on each
//! (Enable, Disable, Uninstall, Copy Extension ID), and Install from VSIX / Install Extension
//! from Location in the "..." menu. Clicking one opens its page: a header (name, publisher,
//! version, description, what it contributes, Enable/Disable and Uninstall) over its README,
//! drawn by the Markdown preview.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use extensions::gallery::{GalleryExtension, Source};
use extensions::Extension;
use render::{Canvas, Image, Rect, TextStyle};

use super::marketplace::Results;
use super::notifications::Severity;
use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, SMALL, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::TextField;

const SEARCH_H: f32 = 34.0;
const ROW_H: f32 = 62.0;
const SECTION_H: f32 = 22.0;
const MESSAGE_H: f32 = 68.0;
const ICON: f32 = 42.0;
/// The extension page's header, above the README.
pub(super) const PAGE_HEADER_H: f32 = 196.0;

#[derive(Default)]
pub(super) struct ExtensionsView {
    pub search: TextField,
    scroll: f32,
    /// Decoded `icon` images, by extension id (None: none or unreadable).
    icons: HashMap<String, Option<Arc<Image>>>,
    /// Collapsed sections, by title.
    collapsed: HashSet<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExtHit {
    Search,
    Body,
    Section(usize),
    /// A row (an index into all the sections' items, in order).
    Row(usize),
    Gear(usize),
    Install(usize),
    Update(usize),
    Retry,
    More,
    Refresh,
}

/// The buttons on an extension's page (the page is group `g`'s active tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageHit {
    Enable,
    Disable,
    Uninstall,
    Install,
    Update,
}

/// A gear menu action on an extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtMenu {
    Enable,
    Disable,
    Uninstall,
    CopyId,
    RevealFolder,
}

/// An extension in the view: installed, or from the marketplace.
#[derive(Clone)]
pub(super) enum Item {
    Installed(Extension),
    Gallery(GalleryExtension),
}

impl Item {
    pub fn id(&self) -> String {
        match self {
            Item::Installed(e) => e.id.clone(),
            Item::Gallery(g) => g.id(),
        }
    }

    pub fn gallery(&self) -> Option<&GalleryExtension> {
        match self {
            Item::Gallery(g) => Some(g),
            Item::Installed(_) => None,
        }
    }
}

/// A section of the view: INSTALLED, ORBVANE (Orbvane's registry), POPULAR or MARKETPLACE (Open
/// VSX).
pub(super) struct ExtSection {
    title: &'static str,
    pub items: Vec<Item>,
    /// Shown instead of items (searching, nothing found, an error).
    message: Option<String>,
    /// A failed request: offer to retry.
    failed: bool,
    count: usize,
}

/// 1234 → "1K", 1234567 → "1.2M".
pub(super) fn short_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", (n / 100_000) as f64 / 10.0)
    } else if n >= 1000 {
        format!("{}K", n / 1000)
    } else {
        n.to_string()
    }
}

impl Workbench {
    /// The installed extensions matching `words` and `filter` (see `installed_filter`).
    fn installed_matching(&self, words: &str, filter: Option<&str>) -> Vec<Extension> {
        let updates = &self.marketplace.updates;
        crate::contributions::with(|r| {
            r.all
                .iter()
                .filter(|e| words.is_empty() || [&e.display_name, &e.id, &e.description, &e.publisher].iter().any(|s| s.to_lowercase().contains(words)))
                .filter(|e| match filter {
                    Some("updates") => updates.contains_key(&e.id),
                    Some("enabled") => r.is_enabled(&e.id),
                    Some("disabled") => !r.is_enabled(&e.id),
                    _ => true,
                })
                .cloned()
                .collect()
        })
    }

    fn results_section(title: &'static str, results: Option<&Results>, loading: &str) -> ExtSection {
        let mut s = ExtSection { title, items: Vec::new(), message: None, failed: false, count: 0 };
        let what = if title == "ORBVANE" { "Orbvane's extension registry" } else { "the marketplace" };
        match results {
            None | Some(Results::Loading) => s.message = Some(loading.into()),
            Some(Results::Failed(e)) => {
                s.message = Some(format!("Couldn't reach {what}: {e}"));
                s.failed = true;
            }
            Some(Results::Done(page)) => {
                s.items = page.extensions.iter().cloned().map(Item::Gallery).collect();
                s.count = page.total as usize;
                if s.items.is_empty() {
                    s.message = Some("No extensions found.".into());
                }
            }
        }
        s
    }

    /// What the view shows: a search of both marketplaces (Orbvane's registry first), the
    /// installed extensions (filtered with `@installed`, `@updates`, `@enabled`, `@disabled`),
    /// or INSTALLED, ORBVANE and POPULAR.
    pub(super) fn extension_sections(&self) -> Vec<ExtSection> {
        if let Some(query) = self.marketplace_query() {
            let pending = self.marketplace.query.as_ref() != Some(&query);
            let results = if pending { None } else { self.marketplace.results.as_ref() };
            let native = self.catalog_results(&query);
            return vec![
                Self::results_section("ORBVANE", native.as_ref(), "Loading..."),
                Self::results_section("MARKETPLACE", results, "Searching the marketplace..."),
            ];
        }
        let (words, filter) = self.installed_filter();
        let installed = self.installed_matching(&words, filter);
        let title = match filter {
            Some("updates") => "OUTDATED",
            Some("enabled") => "ENABLED",
            Some("disabled") => "DISABLED",
            _ => "INSTALLED",
        };
        let message = installed.is_empty().then(|| match filter {
            None => "No extensions installed. Search the marketplace above, or install one from a VSIX file or a folder with the \"...\" menu.".to_string(),
            Some("updates") => "All extensions are up to date.".to_string(),
            _ => "No extensions found.".to_string(),
        });
        let mut sections = vec![ExtSection { title, count: installed.len(), items: installed.into_iter().map(Item::Installed).collect(), message, failed: false }];
        if self.extensions.search.text.trim().is_empty() {
            sections.push(Self::results_section("ORBVANE", self.catalog_results("").as_ref(), "Loading..."));
            sections.push(Self::results_section("POPULAR", self.marketplace.popular.as_ref(), "Loading..."));
        }
        sections
    }

    fn extension_items(&self) -> Vec<Item> {
        self.extension_sections().into_iter().flat_map(|s| if self.extensions.collapsed.contains(s.title) { Vec::new() } else { s.items }).collect()
    }

    fn extension_icon(&mut self, e: &Extension) -> Option<Arc<Image>> {
        self.extensions
            .icons
            .entry(e.id.clone())
            .or_insert_with(|| {
                let rel = e.manifest["icon"].as_str()?;
                let bytes = std::fs::read(e.file(rel)).ok()?;
                crate::imageio::decode(&bytes).ok().map(Arc::new)
            })
            .clone()
    }

    /// An item's icon: the installed copy's, else the marketplace's.
    fn item_icon(&mut self, item: &Item) -> Option<Arc<Image>> {
        let installed = crate::contributions::with(|r| r.get(&item.id()).cloned());
        match (installed, item) {
            (Some(e), _) => self.extension_icon(&e),
            (None, Item::Gallery(g)) => g.icon.clone().and_then(|u| self.gallery_icon(&u)),
            (None, Item::Installed(_)) => None,
        }
    }

    fn draw_item_icon(&mut self, c: &mut Canvas, item: &Item, r: Rect, dim: bool) {
        match self.item_icon(item) {
            Some(img) => c.image(r, &img),
            None => {
                let fg = self.color("icon.foreground").with_alpha(if dim { 0.4 } else { 0.8 });
                c.icon_in(&icons::EXTENSIONS, r, r.w * 0.75, fg);
            }
        }
    }

    pub(super) fn draw_extensions_header_actions(&mut self, c: &mut Canvas, header: Rect) {
        let fg = self.color("icon.foreground");
        self.icon_button(c, Rect::new(header.right() - 32.0, header.y + 6.0, 24.0, 22.0), &icons::ELLIPSIS, Hit::Ext(ExtHit::More), fg);
        self.icon_button(c, Rect::new(header.right() - 58.0, header.y + 6.0, 24.0, 22.0), &icons::REFRESH, Hit::Ext(ExtHit::Refresh), fg);
    }

    /// A button in a row or on a page; returns its width. `prominent` is the blue one.
    fn extension_button(&mut self, c: &mut Canvas, x: f32, y: f32, h: f32, label: &str, prominent: bool, hit: Option<Hit>) -> f32 {
        let (bg, fg, hover) = if prominent {
            ("extensionButton.prominentBackground", "extensionButton.prominentForeground", "extensionButton.prominentHoverBackground")
        } else {
            ("extensionButton.background", "extensionButton.foreground", "extensionButton.hoverBackground")
        };
        let st = TextStyle::ui(12.0, self.color(fg));
        let w = c.measure(label, &st) + 14.0;
        let r = Rect::new(x, y, w, h);
        let hovered = hit.is_some_and(|h| self.hovered(h));
        let mut bg = self.color(if hovered { hover } else { bg });
        if hit.is_none() {
            bg = bg.with_alpha(0.6);
        }
        c.fill_rounded(r, bg, 2.0);
        let border = self.color("extensionButton.border");
        if border.a > 0.0 && !prominent {
            c.bordered(r, bg, border, 1.0, 2.0);
        }
        c.text_in(Rect::new(x + 7.0, y, w - 14.0, h), label, &st);
        if let Some(hit) = hit {
            self.hits.push((r, hit));
        }
        w
    }

    /// The verified publisher badge, `size` wide at (x, y).
    fn draw_verified(&self, c: &mut Canvas, x: f32, y: f32, size: f32) {
        let r = Rect::new(x, y, size, size);
        c.icon_in(&icons::VERIFIED, r, size, self.color_or("extensionIcon.verifiedForeground", "textLink.foreground"));
        c.icon_in(&icons::CHECK, Rect::new(x + size * 0.2, y + size * 0.2, size * 0.6, size * 0.6), size * 0.6, self.color("sideBar.background"));
    }

    /// Stars for a rating out of 5; returns the width drawn.
    fn draw_stars(&self, c: &mut Canvas, x: f32, y: f32, size: f32, rating: f64) -> f32 {
        let color = self.color("extensionIcon.starForeground");
        for i in 0..5 {
            let r = Rect::new(x + i as f32 * (size + 2.0), y, size, size);
            let left = rating - i as f64;
            c.icon_in(&icons::STAR_EMPTY, r, size, color);
            if left >= 0.75 {
                c.icon_in(&icons::STAR_FULL, r, size, color);
            } else if left >= 0.25 {
                c.icon_in(&icons::STAR_HALF, r, size, color);
            }
        }
        5.0 * (size + 2.0)
    }

    pub(super) fn draw_extensions_view(&mut self, c: &mut Canvas, r: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        // The search box.
        let (search_r, rest) = r.cut_top(SEARCH_H);
        let field = Rect::new(search_r.x + 12.0, search_r.y + 5.0, search_r.w - 24.0, 24.0);
        let focused = self.focus == Focus::Extensions && self.palette.is_none();
        let border = if focused { self.color("focusBorder") } else { self.color_or("input.border", "input.background") };
        c.bordered(field, self.color("input.background"), border, 1.0, 2.0);
        let caret_on = self.editor_caret_on();
        let (ifg, ph, sel) = (self.color("input.foreground"), self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        c.push_clip(field);
        self.extensions.search.draw(c, Rect::new(field.x + 6.0, field.y, field.w - 12.0, field.h), &TextStyle::ui(UI, ifg), "Search Extensions in Marketplace", ph, focused, caret_on, sel);
        c.pop_clip();
        self.hits.push((field, Hit::Ext(ExtHit::Search)));
        self.hits.push((rest, Hit::Ext(ExtHit::Body)));

        if self.extensions.search.text.trim().is_empty() && !self.extensions.collapsed.contains("POPULAR") {
            self.ensure_popular();
        }
        if !self.extensions.collapsed.contains("ORBVANE") && (self.extensions.search.text.trim().is_empty() || self.marketplace_query().is_some()) {
            self.ensure_catalog();
        }
        let sections = self.extension_sections();
        let total: f32 = sections
            .iter()
            .map(|s| {
                SECTION_H
                    + match (self.extensions.collapsed.contains(s.title), &s.message) {
                        (true, _) => 0.0,
                        (false, Some(_)) => MESSAGE_H,
                        (false, None) => s.items.len() as f32 * ROW_H,
                    }
            })
            .sum();
        self.extensions.scroll = self.extensions.scroll.clamp(0.0, (total - rest.h).max(0.0));
        let open_page = self.groups.get(self.active_group).and_then(|g| g.tabs.get(g.active)).and_then(|t| t.markdown.as_ref()).and_then(|m| m.extension.clone());
        c.push_clip(rest);
        let mut y = rest.y - self.extensions.scroll;
        let mut index = 0;
        for (si, section) in sections.iter().enumerate() {
            let header = Rect::new(rest.x, y, rest.w, SECTION_H);
            let open = !self.extensions.collapsed.contains(section.title);
            self.section_header(c, header, section.title, open, Hit::Ext(ExtHit::Section(si)));
            if section.message.is_none() || section.count > 0 {
                let style = TextStyle::ui(SMALL, fg);
                let label = section.count.to_string();
                let badge_w = (c.measure(&label, &style) + 10.0).max(18.0);
                self.badge(c, header.right() - 14.0 - badge_w, header.y + 3.0, section.count);
            }
            y += SECTION_H;
            if !open {
                continue;
            }
            if let Some(msg) = &section.message {
                let st = TextStyle::ui(UI, self.color("descriptionForeground"));
                let lines = super::intel::wrap(c, msg, &st, rest.w - 40.0);
                for (i, line) in lines.iter().take(3).enumerate() {
                    c.text(rest.x + 20.0, y + 8.0 + i as f32 * 20.0, line, &st);
                }
                if section.failed {
                    let link = TextStyle::ui(UI, self.color("textLink.foreground"));
                    let lr = Rect::new(rest.x + 20.0, y + 8.0 + lines.len().min(3) as f32 * 20.0 - 2.0, 40.0, 20.0);
                    c.text_in(lr, "Retry", &link);
                    self.hits.push((lr, Hit::Ext(ExtHit::Retry)));
                }
                y += MESSAGE_H;
                continue;
            }
            for item in &section.items {
                let row = Rect::new(rest.x, y, rest.w, ROW_H);
                y += ROW_H;
                let i = index;
                index += 1;
                if row.bottom() < rest.y || row.y > rest.bottom() {
                    continue;
                }
                self.draw_extension_row(c, row, i, item, open_page.as_deref());
            }
        }
        c.pop_clip();
    }

    fn draw_extension_row(&mut self, c: &mut Canvas, row: Rect, i: usize, item: &Item, open_page: Option<&str>) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let id = item.id();
        let installed = crate::contributions::with(|r| r.get(&id).cloned());
        let enabled = crate::contributions::with(|r| r.is_enabled(&id));
        let hovered = [ExtHit::Row(i), ExtHit::Gear(i), ExtHit::Install(i), ExtHit::Update(i)].into_iter().any(|h| self.hovered(Hit::Ext(h)));
        if open_page == Some(id.as_str()) {
            c.fill(row, self.color("list.inactiveSelectionBackground"));
        } else if hovered {
            c.fill(row, self.color("list.hoverBackground"));
        }
        self.hits.push((row, Hit::Ext(ExtHit::Row(i))));
        let dim_it = installed.is_some() && !enabled;
        let alpha = if dim_it { 0.55 } else { 1.0 };
        self.draw_item_icon(c, item, Rect::new(row.x + 14.0, row.y + 10.0, ICON, ICON), dim_it);
        let x = row.x + 14.0 + ICON + 12.0;
        let w = row.right() - x - 10.0;
        let name_st = TextStyle::ui(13.0, fg.with_alpha(alpha)).weight(700);
        let dim = TextStyle::ui(12.0, self.color("descriptionForeground").with_alpha(alpha));
        let (name, version, description, publisher) = match item {
            Item::Installed(e) => (e.display_name.clone(), e.version.clone(), e.description.clone(), e.publisher.clone()),
            Item::Gallery(g) => (g.display_name.clone(), String::new(), g.description.clone(), g.publisher.clone()),
        };
        // Installs and rating, on the right of the name (marketplace rows).
        let mut stats_w = 0.0;
        if let Some(g) = item.gallery().filter(|g| g.source == Source::OpenVsx) {
            let mut sx = row.right() - 10.0;
            if let Some(rating) = g.rating {
                let label = format!("{rating:.1}");
                let lw = c.measure(&label, &dim);
                sx -= lw;
                c.text_in(Rect::new(sx, row.y + 5.0, lw + 1.0, 18.0), &label, &dim);
                sx -= 16.0;
                c.icon_in(&icons::STAR_FULL, Rect::new(sx, row.y + 7.0, 14.0, 14.0), 13.0, self.color("extensionIcon.starForeground"));
                sx -= 6.0;
            }
            let label = short_count(g.downloads);
            let lw = c.measure(&label, &dim);
            sx -= lw;
            c.text_in(Rect::new(sx, row.y + 5.0, lw + 1.0, 18.0), &label, &dim);
            sx -= 17.0;
            c.icon_in(&icons::CLOUD_DOWNLOAD, Rect::new(sx, row.y + 6.0, 16.0, 16.0), 15.0, self.color("icon.foreground"));
            stats_w = row.right() - 10.0 - sx + 6.0;
        }
        c.push_clip(Rect::new(x, row.y, w, row.h));
        let name = elide(c, &name, &name_st, w - stats_w);
        let nw = c.text_in(Rect::new(x, row.y + 4.0, w - stats_w, 20.0), &name, &name_st);
        if !version.is_empty() && nw + 6.0 < w - stats_w {
            c.text_in(Rect::new(x + nw + 6.0, row.y + 5.0, w - stats_w - nw - 6.0, 20.0), &version, &dim);
        }
        let desc_st = TextStyle::ui(13.0, fg.with_alpha(0.9 * alpha));
        let desc = elide(c, &description, &desc_st, w - 4.0);
        c.text_in(Rect::new(x, row.y + 22.0, w - 4.0, 18.0), &desc, &desc_st);
        c.pop_clip();

        // The action on the right of the last line: Install, Update, or the gear.
        let update = self.marketplace.updates.contains_key(&id);
        let busy = self.marketplace.installing.contains(&id);
        let mut right = row.right() - 8.0;
        let by = row.y + 39.0;
        let st = TextStyle::ui(12.0, self.color("extensionButton.prominentForeground"));
        if busy {
            let label = if installed.is_some() { "Updating" } else { "Installing" };
            right -= c.measure(label, &st) + 14.0;
            self.extension_button(c, right, by, 18.0, label, true, None);
        } else if installed.is_none() {
            right -= c.measure("Install", &st) + 14.0;
            self.extension_button(c, right, by, 18.0, "Install", true, Some(Hit::Ext(ExtHit::Install(i))));
        } else {
            if hovered || open_page == Some(id.as_str()) || update {
                let gear = Rect::new(right - 22.0, row.y + 38.0, 22.0, 20.0);
                self.icon_button(c, gear, &icons::GEAR, Hit::Ext(ExtHit::Gear(i)), self.color("icon.foreground"));
                right -= 26.0;
            }
            if update {
                right -= c.measure("Update", &st) + 14.0;
                self.extension_button(c, right, by, 18.0, "Update", true, Some(Hit::Ext(ExtHit::Update(i))));
            }
        }
        let verified = matches!(item, Item::Gallery(g) if g.verified);
        // Open VSX extensions: only their package.json's contributions work here.
        let tag = match (&installed, item) {
            (Some(e), _) if e.has_js_code() => Some("Contributions only"),
            (None, Item::Gallery(g)) if g.source == Source::OpenVsx => Some("Open VSX"),
            _ => None,
        };
        let publisher = match (&installed, enabled) {
            (Some(_), false) => format!("{publisher} (Disabled)"),
            (Some(e), true) if e.linked => format!("{publisher} (from {})", e.path.display()),
            _ => publisher,
        };
        let tag = tag.map(|t| format!("·  {t}"));
        let tag_w = tag.as_ref().map_or(0.0, |t| c.measure(t, &dim) + 8.0);
        let pw = right - x - 6.0 - if verified { 18.0 } else { 0.0 } - tag_w;
        let publisher = elide(c, &publisher, &dim, pw.max(0.0));
        c.push_clip(Rect::new(x, row.y, (right - x).max(0.0), row.h));
        let mut tw = c.text_in(Rect::new(x, row.y + 40.0, pw.max(0.0), 18.0), &publisher, &dim);
        if verified {
            self.draw_verified(c, x + tw + 4.0, row.y + 42.0, 13.0);
            tw += 18.0;
        }
        if let Some(tag) = &tag {
            c.text_in(Rect::new(x + tw + 8.0, row.y + 40.0, tag_w, 18.0), tag, &dim);
        }
        c.pop_clip();
    }

    pub(super) fn scroll_extensions(&mut self, dy: f32) {
        self.extensions.scroll = (self.extensions.scroll - dy).max(0.0);
    }

    pub(super) fn extensions_click(&mut self, hit: ExtHit, x: f32, y: f32) {
        let item = |wb: &Self, i: usize| wb.extension_items().into_iter().nth(i);
        match hit {
            ExtHit::Search => {
                self.focus = Focus::Extensions;
                self.extensions.search.click(x, false);
            }
            ExtHit::Body => self.focus = Focus::Extensions,
            ExtHit::Section(si) => {
                if let Some(title) = self.extension_sections().get(si).map(|s| s.title) {
                    if !self.extensions.collapsed.remove(title) {
                        self.extensions.collapsed.insert(title);
                    }
                }
            }
            ExtHit::Row(i) => match item(self, i) {
                Some(Item::Installed(e)) => self.open_extension_page(&e.id),
                Some(Item::Gallery(g)) => self.open_gallery_page(&g),
                None => {}
            },
            ExtHit::Gear(i) => {
                if let Some(it) = item(self, i) {
                    self.extension_menu(&it.id(), x, y);
                }
            }
            ExtHit::Install(i) => {
                if let Some(it) = item(self, i) {
                    self.install_from_marketplace(&it.id(), false);
                }
            }
            ExtHit::Update(i) => {
                if let Some(it) = item(self, i) {
                    self.install_from_marketplace(&it.id(), true);
                }
            }
            ExtHit::Retry | ExtHit::Refresh => self.refresh_marketplace(),
            ExtHit::More => {
                use crate::commands::Command;
                let group = |cs: &[Command]| cs.iter().map(|c| (PopupItem::Item { label: c.menu_label().to_string(), enabled: true, checked: None }, PopupAction::Run(*c))).collect::<Vec<_>>();
                let mut entries = group(&[Command::ExtensionsCheckForUpdates, Command::ExtensionsUpdateAll]);
                entries.push((PopupItem::Separator, PopupAction::None));
                entries.extend(group(&[Command::ExtensionsInstallVsix, Command::ExtensionsInstallFromLocation, Command::ExtensionsOpenFolder, Command::RestartExtensionHost]));
                self.show_popup(entries, x, y);
            }
        }
    }

    pub(super) fn extensions_key(&mut self, k: &KeyInput) {
        match k.key {
            Key::Escape if !self.extensions.search.text.is_empty() => self.extensions.search.set_text(""),
            Key::Escape => self.focus = Focus::Editor,
            _ => {
                self.extensions.search.key(k);
                self.extensions.scroll = 0.0;
            }
        }
    }

    pub(super) fn extensions_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        if all {
            return self.extensions.search.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.extensions.search.insert(&text);
            }
            return;
        }
        let f = &mut self.extensions.search;
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    fn extension_menu(&mut self, id: &str, x: f32, y: f32) {
        let Some(e) = crate::contributions::with(|r| r.get(id).cloned()) else { return };
        let enabled = crate::contributions::with(|r| r.is_enabled(id));
        let item = |label: &str, m: ExtMenu| (PopupItem::Item { label: label.into(), enabled: true, checked: None }, PopupAction::Extension(e.id.clone(), m));
        let mut entries = vec![if enabled { item("Disable", ExtMenu::Disable) } else { item("Enable", ExtMenu::Enable) }, item("Uninstall", ExtMenu::Uninstall)];
        entries.push((PopupItem::Separator, PopupAction::None));
        entries.push(item("Copy Extension ID", ExtMenu::CopyId));
        entries.push(item("Reveal in Finder", ExtMenu::RevealFolder));
        self.show_popup(entries, x, y);
    }

    pub(super) fn extension_menu_action(&mut self, id: &str, action: ExtMenu) {
        match action {
            ExtMenu::Enable => self.set_extension_enabled(id, true),
            ExtMenu::Disable => self.set_extension_enabled(id, false),
            ExtMenu::Uninstall => self.uninstall_extension(id),
            ExtMenu::CopyId => {
                if let Some(cb) = &mut self.clipboard {
                    let _ = cb.set_text(id.to_string());
                }
            }
            ExtMenu::RevealFolder => {
                if let Some(path) = crate::contributions::with(|r| r.get(id).map(|e| e.path.clone())) {
                    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
                }
            }
        }
    }

    /// After extensions were installed, removed, enabled or disabled: keybindings, JSON schemas
    /// and the Settings editor pick up what they contribute.
    fn extensions_changed(&mut self) {
        self.reload_keymap(true);
        let themes: Vec<String> = self.themes().into_iter().map(|t| t.name).collect();
        self.lsp.set_builtin_options("builtin:json", crate::json_schemas::associations(&themes));
        self.extensions.icons.clear();
    }

    /// Languages only change at startup.
    fn notify_languages_need_restart(&mut self, e: &Extension, verb: &str) {
        if !e.languages().is_empty() {
            let msg = format!("Restart Orbvane to {verb} the languages of '{}'.", e.display_name);
            self.notify(Severity::Info, &msg, "Extensions", Vec::new(), None);
        }
    }

    pub(super) fn set_extension_enabled(&mut self, id: &str, enabled: bool) {
        let result = crate::contributions::with_mut(|r| r.set_enabled(id, enabled));
        if let Err(e) = result {
            return self.notify(Severity::Error, &e, "Extensions", Vec::new(), None);
        }
        let Some(e) = crate::contributions::with(|r| r.get(id).cloned()) else { return };
        if enabled {
            crate::contributions::register(&e);
            self.ext_activate_new(&e);
            self.notify_languages_need_restart(&e, "use");
        } else {
            self.ext_stop(id);
            crate::commands::set_ext_commands_enabled(id, false);
            self.notify_languages_need_restart(&e, "stop using");
        }
        self.extensions_changed();
    }

    /// An extension that was just installed or enabled starts if its events already happened.
    fn ext_activate_new(&mut self, e: &Extension) {
        let events = e.activation_events();
        let happened = events.iter().any(|a| a == "*" || a == "onStartupFinished")
            || events.iter().any(|a| a.strip_prefix("onLanguage:").is_some_and(|l| self.docs.iter().flatten().any(|d| d.lang.id() == l)));
        if happened && e.program().is_some() {
            self.ext_activate_one(e);
        }
    }

    pub(super) fn uninstall_extension(&mut self, id: &str) {
        let Some(e) = crate::contributions::with(|r| r.get(id).cloned()) else { return };
        self.ext_stop(id);
        crate::commands::set_ext_commands_enabled(id, false);
        if let Err(err) = crate::contributions::with_mut(|r| r.uninstall(id)) {
            return self.notify(Severity::Error, &err, "Extensions", Vec::new(), None);
        }
        self.close_extension_page(id);
        self.marketplace.updates.remove(id);
        self.notify_languages_need_restart(&e, "stop using");
        self.extensions_changed();
    }

    /// Extensions: Install from VSIX...
    pub(super) fn install_vsix(&mut self) {
        let Some(path) = self.file_dialog().set_title("Install from VSIX").add_filter("VSIX Extensions", &["vsix"]).pick_file() else { return };
        let result = crate::contributions::with_mut(|r| r.install_vsix(&path));
        self.extension_installed(result, Some(&format!(" from {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())));
    }

    /// Developer: Install Extension from Location... (a folder with a package.json, used where it is).
    pub(super) fn install_extension_folder(&mut self) {
        let Some(path) = self.file_dialog().set_title("Install Extension from Location").pick_folder() else { return };
        let result = crate::contributions::with_mut(|r| r.install_folder(&path));
        self.extension_installed(result, Some(&format!(" from {}", path.display())));
    }

    /// After an install: `from` ends the "Completed installing" message (installs from the
    /// marketplace are None: they happen in the view, which shows them).
    pub(super) fn extension_installed(&mut self, result: Result<String, String>, from: Option<&str>) {
        let id = match result {
            Ok(id) => id,
            Err(e) => return self.notify(Severity::Error, &e, "Extensions", Vec::new(), None),
        };
        let Some(e) = crate::contributions::with(|r| r.get(&id).cloned()) else { return };
        // A new version replaces the running one.
        self.ext_stop(&id);
        crate::contributions::register(&e);
        if crate::contributions::with(|r| r.is_enabled(&id)) {
            self.ext_activate_new(&e);
        }
        self.extensions_changed();
        self.notify_languages_need_restart(&e, "use");
        let Some(from) = from else { return };
        self.notify(Severity::Info, &format!("Completed installing extension '{}'{from}.", e.display_name), "Extensions", Vec::new(), None);
        self.show_view(super::View::Extensions);
        self.open_extension_page(&id);
    }

    /// Developer: Restart Extension Host: stops every extension's program and reads the
    /// installed extensions again (after rebuilding one).
    pub(super) fn restart_extensions(&mut self) {
        self.ext_shutdown();
        let before: Vec<String> = crate::contributions::enabled().iter().map(|e| e.id.clone()).collect();
        crate::contributions::with_mut(|r| *r = extensions::Registry::scan(&crate::contributions::dir()));
        for id in before {
            crate::commands::set_ext_commands_enabled(&id, false);
        }
        for e in crate::contributions::enabled() {
            crate::contributions::register(&e);
            self.ext_activate_new(&e);
        }
        self.extensions_changed();
    }

    pub(super) fn reveal_extensions_folder(&mut self) {
        let dir = crate::contributions::dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("open").arg(&dir).spawn();
    }

    // ------------------------------------------------------------------ the page

    /// Opens an installed extension's page (its README under a header) in the active group.
    pub(super) fn open_extension_page(&mut self, id: &str) {
        let Some(e) = crate::contributions::with(|r| r.get(id).cloned()) else { return };
        let readme = e.readme().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_else(|| "No README available.".to_string());
        self.open_page_tab(&e.id, &e.display_name, &readme, Some(e.path.clone()));
    }

    /// Opens a marketplace extension's page; its README is fetched.
    pub(super) fn open_gallery_page(&mut self, g: &GalleryExtension) {
        let id = g.id();
        if crate::contributions::with(|r| r.get(&id).is_some()) {
            return self.open_extension_page(&id);
        }
        if self.open_page_tab(&id, &g.display_name, "Loading...", None) {
            self.fetch_extension_page(g);
        }
    }

    /// Shows the page tab of extension `id`, making one if needed (it replaces the page of
    /// another extension, like the preview editors). Returns whether it's new.
    fn open_page_tab(&mut self, id: &str, name: &str, text: &str, base: Option<std::path::PathBuf>) -> bool {
        let g = self.active_group;
        if let Some(i) = self.groups[g].tabs.iter().position(|t| t.markdown.as_ref().is_some_and(|m| m.extension.as_deref() == Some(id))) {
            self.groups[g].active = i;
            return false;
        }
        let mut doc = crate::editor::Doc::open_virtual(&format!("Extension: {name}.md"), text);
        doc.label = Some(format!("Extension: {name}"));
        doc.buffer.mark_saved();
        let doc = self.add_doc(doc);
        let mut ed = crate::editor::EditorState::new(doc);
        let mut preview = super::markdown_view::Preview::new();
        preview.extension = Some(id.to_string());
        preview.base = base;
        ed.markdown = Some(Box::new(preview));
        let group = &mut self.groups[g];
        match group.tabs.iter().position(|t| t.markdown.as_ref().is_some_and(|m| m.extension.is_some())) {
            Some(i) => {
                group.tabs[i] = ed;
                group.active = i;
            }
            None => {
                let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
                group.tabs.insert(at, ed);
                group.active = at;
            }
        }
        self.focus = Focus::Editor;
        true
    }

    /// Replaces the README shown on extension `id`'s page (once fetched).
    pub(super) fn set_extension_page_text(&mut self, id: &str, text: &str) {
        let docs: Vec<usize> = self.groups.iter().flat_map(|g| &g.tabs).filter(|t| t.markdown.as_ref().is_some_and(|m| m.extension.as_deref() == Some(id))).map(|t| t.doc).collect();
        for d in docs {
            if let Some(doc) = self.docs[d].as_mut() {
                let end = doc.buffer.end();
                doc.buffer.edit(&[], &[(text::Pos::new(0, 0), end, text)], text::EditKind::Other);
                doc.buffer.mark_saved();
            }
        }
    }

    fn close_extension_page(&mut self, id: &str) {
        for g in &mut self.groups {
            if let Some(i) = g.tabs.iter().position(|t| t.markdown.as_ref().is_some_and(|m| m.extension.as_deref() == Some(id))) {
                g.tabs.remove(i);
                g.active = g.active.min(g.tabs.len().saturating_sub(1));
            }
        }
    }

    /// The page's header, `PAGE_HEADER_H` tall from `top`: an installed extension, or one from
    /// the marketplace.
    pub(super) fn draw_extension_page_header(&mut self, c: &mut Canvas, g: usize, id: &str, r: Rect, top: f32) {
        let installed = crate::contributions::with(|reg| reg.get(id).cloned());
        let gallery = self.marketplace.updates.get(id).or_else(|| self.marketplace.known.get(id)).cloned();
        let item = match (&installed, &gallery) {
            (Some(e), _) => Item::Installed(e.clone()),
            (None, Some(ge)) => Item::Gallery(ge.clone()),
            (None, None) => return,
        };
        let enabled = crate::contributions::with(|reg| reg.is_enabled(id));
        let fg = self.color("editor.foreground");
        let dim = self.color("descriptionForeground");
        let x0 = r.x + 26.0;
        let icon = Rect::new(x0, top + 22.0, 100.0, 100.0);
        self.draw_item_icon(c, &item, icon, installed.is_some() && !enabled);
        let x = icon.right() + 20.0;
        let w = r.right() - x - 20.0;
        let (name, version, publisher, description) = match (&installed, &gallery) {
            (Some(e), _) => (e.display_name.clone(), e.version.clone(), e.publisher.clone(), e.description.clone()),
            (None, Some(ge)) => (ge.display_name.clone(), ge.version.clone(), ge.publisher.clone(), ge.description.clone()),
            _ => return,
        };
        let name_w = c.text_in(Rect::new(x, top + 18.0, w, 34.0), &name, &TextStyle::ui(26.0, fg).weight(600));
        let tag = TextStyle::ui(12.0, dim);
        let tag_x = x + name_w + 10.0;
        let tag_w = c.measure(&format!("v{version}"), &tag) + 10.0;
        c.fill_rounded(Rect::new(tag_x, top + 27.0, tag_w, 18.0), self.color("badge.background").with_alpha(0.5), 3.0);
        c.text_in(Rect::new(tag_x + 5.0, top + 27.0, 100.0, 18.0), &format!("v{version}"), &tag);

        // Publisher (verified), installs, rating, id.
        let info_st = TextStyle::ui(13.0, dim);
        let mut ix = x;
        let iy = top + 54.0;
        ix += c.text_in(Rect::new(ix, iy, w, 20.0), &publisher, &info_st);
        if gallery.as_ref().is_some_and(|ge| ge.verified) {
            self.draw_verified(c, ix + 4.0, iy + 3.0, 14.0);
            ix += 20.0;
        }
        let sep = |c: &mut Canvas, ix: &mut f32| {
            *ix += c.text_in(Rect::new(*ix, iy, 30.0, 20.0), "  |  ", &info_st);
        };
        if let Some(ge) = gallery.as_ref().filter(|ge| ge.source == Source::OpenVsx) {
            sep(c, &mut ix);
            c.icon_in(&icons::CLOUD_DOWNLOAD, Rect::new(ix, iy + 2.0, 16.0, 16.0), 15.0, self.color("icon.foreground"));
            ix += 20.0;
            ix += c.text_in(Rect::new(ix, iy, 120.0, 20.0), &thousands(ge.downloads), &info_st);
            if let Some(rating) = ge.rating {
                sep(c, &mut ix);
                ix += self.draw_stars(c, ix, iy + 3.0, 14.0, rating);
                if ge.reviews > 0 {
                    ix += c.text_in(Rect::new(ix + 2.0, iy, 60.0, 20.0), &format!("({})", ge.reviews), &info_st) + 2.0;
                }
            }
        }
        let mut rest = vec![id.to_string()];
        if let Some(e) = installed.as_ref().filter(|e| e.linked) {
            rest.push(format!("from {}", e.path.display()));
        }
        if self.ext_running(id) {
            rest.push("running".into());
        }
        sep(c, &mut ix);
        c.text_in(Rect::new(ix, iy, (r.right() - ix - 20.0).max(0.0), 20.0), &rest.join("  |  "), &info_st);
        c.text_in(Rect::new(x, top + 76.0, w, 20.0), &description, &TextStyle::ui(13.0, fg));

        // Buttons.
        let busy = self.marketplace.installing.contains(id);
        let mut buttons: Vec<(&str, bool, Option<PageHit>)> = Vec::new();
        match &installed {
            None if busy => buttons.push(("Installing", true, None)),
            None => buttons.push(("Install", true, Some(PageHit::Install))),
            Some(_) => {
                if busy {
                    buttons.push(("Updating", true, None));
                } else if self.marketplace.updates.contains_key(id) {
                    buttons.push(("Update", true, Some(PageHit::Update)));
                }
                buttons.push(if enabled { ("Disable", false, Some(PageHit::Disable)) } else { ("Enable", false, Some(PageHit::Enable)) });
                buttons.push(("Uninstall", false, Some(PageHit::Uninstall)));
            }
        }
        let mut bx = x;
        for (label, prominent, hit) in buttons {
            let label = match (label, &self.marketplace.updates.get(id)) {
                ("Update", Some(u)) => format!("Update to v{}", u.version),
                _ => label.to_string(),
            };
            bx += self.extension_button(c, bx, top + 104.0, 24.0, &label, prominent, hit.map(|h| Hit::ExtPage(g, h))) + 8.0;
        }

        // What it contributes, and whether its code can run here.
        let described = installed.clone().or_else(|| self.marketplace.manifests.get(id).and_then(|m| Extension::from_manifest(m.clone(), std::path::Path::new("")).ok()));
        let mut notes: Vec<String> = Vec::new();
        if let Some(e) = &described {
            let counts = e.contribution_counts();
            if !counts.is_empty() {
                notes.push(format!("Contributes: {}", counts.iter().map(|(what, n)| format!("{what} ({n})")).collect::<Vec<_>>().join(", ")));
            }
        }
        let javascript = match (&installed, self.marketplace.manifests.get(id)) {
            (Some(e), _) => e.has_js_code(),
            (None, Some(m)) => extensions::gallery::runs_javascript(m),
            (None, None) => false,
        };
        let native = crate::contributions::with(|reg| reg.native.contains(id)) || gallery.as_ref().is_some_and(|ge| ge.source == Source::Orbvane);
        if javascript {
            notes.push("Its code is JavaScript, which Orbvane doesn't run; what it contributes in package.json works.".into());
        } else if native {
            let from = gallery.as_ref().and_then(|ge| ge.repository.clone()).map_or(String::new(), |r| format!(" from {r}"));
            notes.push(format!("From Orbvane's extension registry, built from source{from}."));
        } else if let Some(program) = installed.as_ref().and_then(|e| e.program()) {
            if !program.exists() {
                notes.push(format!("Its program {} doesn't exist (build it first).", program.display()));
            }
        }
        if let (None, Some(ge)) = (&installed, &gallery) {
            if !ge.categories.is_empty() && notes.len() < 2 {
                notes.push(format!("Categories: {}", ge.categories.join(", ")));
            }
        }
        for (i, note) in notes.iter().take(2).enumerate() {
            c.text_in(Rect::new(x, top + 138.0 + i as f32 * 20.0, w, 20.0), note, &TextStyle::ui(12.0, dim));
        }
        c.fill(Rect::new(r.x + 20.0, top + PAGE_HEADER_H - 8.0, r.w - 40.0, 1.0), self.color_or("editorWidget.border", "panel.border"));
    }

    pub(super) fn extension_page_click(&mut self, g: usize, hit: PageHit) {
        let Some(id) = self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)).and_then(|t| t.markdown.as_ref()).and_then(|m| m.extension.clone()) else { return };
        match hit {
            PageHit::Enable => self.set_extension_enabled(&id, true),
            PageHit::Disable => self.set_extension_enabled(&id, false),
            PageHit::Uninstall => self.uninstall_extension(&id),
            PageHit::Install => self.install_from_marketplace(&id, false),
            PageHit::Update => self.install_from_marketplace(&id, true),
        }
    }

    /// Starts one extension whose activation already happened.
    fn ext_activate_one(&mut self, e: &Extension) {
        let event = e.activation_events().into_iter().next().unwrap_or_else(|| "*".into());
        self.ext_start(e, &event);
    }
}

/// 1234567 → "1,234,567".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `text` cut to fit `width`, ending in "…" when cut.
fn elide(c: &mut Canvas, text: &str, style: &TextStyle, width: f32) -> String {
    if c.measure(text, style) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if c.measure(&candidate, style) <= width { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}
