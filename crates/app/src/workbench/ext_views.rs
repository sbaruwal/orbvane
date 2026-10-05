//! Extensions' tree views (`contributes.views`): sections in the Explorer, or in an extension's
//! own activity bar container (`View::Ext`). Items come from the extension's program
//! (`treeView/getChildren`, one level at a time, as nodes expand) and are cached until it asks
//! for a refresh (`treeView/refresh`). Title bar buttons and item actions come from the
//! `view/title` and `view/item/context` menus; `when` clauses see `view`, `viewItem` and the
//! keys extensions set with `setContext`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use render::{Canvas, Icon, Image, Rect, TextStyle};
use serde_json::{json, Value};

use super::preferences::PopupAction;
use super::{sections, Hit, PopupItem, View, Workbench, ROW_H, UI};
use crate::contributions;
use crate::icons;

/// The message for a view nobody provides.
const NO_PROVIDER: &str = "There is no data provider registered that can provide view data.";

/// A tree item as the extension described it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TreeItem {
    pub id: String,
    pub label: String,
    pub description: String,
    pub tooltip: String,
    pub icon: Option<String>,
    /// 0 none, 1 collapsed, 2 expanded (at first).
    pub collapsible: u8,
    pub command: Option<(String, Vec<Value>)>,
    pub context: Option<String>,
    /// The item as sent (passed to menu commands, like passes the element).
    pub raw: Value,
}

impl TreeItem {
    fn parse(v: &Value) -> Option<TreeItem> {
        let label = match &v["label"] {
            Value::String(s) => s.clone(),
            l => l["label"].as_str().unwrap_or("").to_string(),
        };
        let id = v["id"].as_str().map(String::from).unwrap_or_else(|| label.clone());
        let icon = match &v["icon"] {
            Value::String(s) => Some(s.clone()),
            o @ Value::Object(_) => o["id"].as_str().map(|i| format!("$({i})")).or_else(|| o["dark"].as_str().map(String::from)),
            _ => None,
        };
        let command = v["command"]["command"].as_str().map(|c| (c.to_string(), v["command"]["arguments"].as_array().cloned().unwrap_or_default()));
        Some(TreeItem {
            id,
            label,
            description: v["description"].as_str().unwrap_or("").to_string(),
            tooltip: v["tooltip"].as_str().unwrap_or("").to_string(),
            icon,
            collapsible: v["collapsibleState"].as_u64().unwrap_or(0) as u8,
            command,
            context: v["contextValue"].as_str().map(String::from),
            raw: v.clone(),
        })
    }
}

enum Children {
    Loading,
    Items(Vec<TreeItem>),
    Failed(String),
}

pub(super) struct TreeState {
    pub open: bool,
    pub height: Option<f32>,
    scroll: f32,
    /// Children by parent item id ("" for the roots).
    children: HashMap<String, Children>,
    /// Expanded items (and collapsed ones that started expanded).
    expanded: HashSet<String>,
    collapsed: HashSet<String>,
    selected: Option<String>,
}

impl Default for TreeState {
    fn default() -> Self {
        TreeState { open: true, height: None, scroll: 0.0, children: HashMap::new(), expanded: HashSet::new(), collapsed: HashSet::new(), selected: None }
    }
}

/// An item icon: a vector icon, or an image file.
#[derive(Clone)]
enum ItemIcon {
    Vector(&'static Icon),
    Image(Arc<Image>),
}

/// A menu entry: (extension, command, title, icon, group, when).
type MenuEntry = (String, String, String, Option<&'static Icon>, Option<String>, Option<String>);

#[derive(Default)]
pub(super) struct ExtViews {
    generation: Option<u64>,
    /// The enabled extensions' views.
    pub list: Vec<contributions::View>,
    title_menu: Vec<MenuEntry>,
    item_menu: Vec<MenuEntry>,
    pub state: HashMap<String, TreeState>,
    images: HashMap<PathBuf, Option<ItemIcon>>,
    /// Context keys set with the `setContext` command.
    pub context: HashMap<String, String>,
}

/// What was hit in an extension view (`Hit::ExtTree(view index, _)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TreeHit {
    Section,
    Body,
    Row(u32),
    /// An inline action of a row.
    Inline(u32, u8),
    /// A title bar button, or the "..." of the rest.
    Title(u8),
    More,
}

/// A visible row: depth and item.
pub(super) struct Row {
    pub depth: usize,
    pub item: TreeItem,
    pub expanded: bool,
}

impl Workbench {
    /// Re-reads the views and menus when the extensions changed.
    pub(super) fn ext_views_sync(&mut self) {
        let generation = contributions::generation();
        if self.ext_views.generation == Some(generation) {
            return;
        }
        self.ext_views.generation = Some(generation);
        self.ext_views.list = contributions::views();
        let entries = |menu: &str| -> Vec<MenuEntry> {
            contributions::menu(menu).into_iter().map(|(ext, item, title, icon)| (ext, item.command, title, icon, item.group, item.when)).collect()
        };
        self.ext_views.title_menu = entries("view/title");
        self.ext_views.item_menu = entries("view/item/context");
        if let View::Ext(i) = self.view {
            if !self.ext_view_containers().contains(&View::Ext(i)) {
                self.view = View::Explorer;
            }
        }
    }

    fn when_context(&self, view: &str, item: Option<&TreeItem>) -> crate::when::Context<'_> {
        let mut ctx: crate::when::Context = self.ext_views.context.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        ctx.insert("view", view.to_string());
        if let Some(v) = item.and_then(|i| i.context.clone()) {
            ctx.insert("viewItem", v);
        }
        ctx
    }

    /// Evaluates a `when` clause, with `config.*` read from the settings.
    fn when_eval(&self, clause: Option<&str>, ctx: &crate::when::Context) -> bool {
        let config = |key: &str| {
            let v = self.settings.get(key.strip_prefix("config.")?);
            match v {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s),
                v => Some(v.to_string()),
            }
        };
        crate::when::eval_with(clause, ctx, &config)
    }

    fn view_visible(&self, v: &contributions::View) -> bool {
        self.when_eval(v.when.as_deref(), &self.when_context(&v.id, None))
    }

    /// The views (indices into `ext_views.list`) shown in `container`.
    pub(super) fn ext_views_in(&self, container: &str) -> Vec<usize> {
        self.ext_views.list.iter().enumerate().filter(|(_, v)| v.container == container && self.view_visible(v)).map(|(i, _)| i).collect()
    }

    /// The extension containers with a visible view, for the activity bar.
    pub(super) fn ext_view_containers(&self) -> Vec<View> {
        let mut out: Vec<View> = Vec::new();
        for v in self.ext_views.list.iter().filter(|v| self.view_visible(v)) {
            if let Some(i) = contributions::container_index(&v.container) {
                if !out.contains(&View::Ext(i)) {
                    out.push(View::Ext(i));
                }
            }
        }
        out
    }

    pub(super) fn ext_tree_state(&mut self, vi: usize) -> &mut TreeState {
        let id = self.ext_views.list[vi].id.clone();
        self.ext_views.state.entry(id).or_default()
    }

    /// Draws container `i`'s views as sections.
    pub(super) fn draw_ext_container(&mut self, c: &mut Canvas, r: Rect, i: u16) {
        let Some(container) = contributions::container(i) else { return };
        let views = self.ext_views_in(&container.id);
        if views.is_empty() {
            return;
        }
        // One view: no section header.
        if views.len() == 1 {
            self.hits.push((r, Hit::ExtTree(views[0] as u16, TreeHit::Body)));
            return self.draw_ext_tree(c, r, views[0]);
        }
        let open: Vec<bool> = views.iter().map(|&v| self.ext_tree_state(v).open).collect();
        let heights: Vec<Option<f32>> = views.iter().map(|&v| self.ext_tree_state(v).height).collect();
        let (heads, bodies) = sections::split(r, &open, &heights);
        for (k, &vi) in views.iter().enumerate() {
            self.draw_ext_view_section(c, heads[k], bodies[k], vi);
        }
    }

    /// A view as a section: its header (with its title buttons) and, when open, its tree.
    pub(super) fn draw_ext_view_section(&mut self, c: &mut Canvas, head: Rect, body: Rect, vi: usize) {
        let name = self.ext_views.list[vi].name.to_uppercase();
        let open = self.ext_tree_state(vi).open;
        self.section_header(c, head, &name, open, Hit::ExtTree(vi as u16, TreeHit::Section));
        if open {
            self.hits.push((body, Hit::ExtTree(vi as u16, TreeHit::Body)));
            self.draw_ext_tree(c, body, vi);
        }
        let (mx, my) = self.mouse;
        if head.contains(mx, my) || body.contains(mx, my) {
            self.draw_ext_title_actions(c, head, vi);
        }
    }

    /// The `view/title` buttons (group `navigation`) and "..." for the rest, at the right of `head`.
    pub(super) fn draw_ext_title_actions(&mut self, c: &mut Canvas, head: Rect, vi: usize) {
        let (buttons, rest) = self.ext_title_entries(vi);
        let fg = self.color_or("sideBarSectionHeader.foreground", "sideBar.foreground");
        let mut x = head.right() - 28.0;
        if !rest.is_empty() {
            self.icon_button(c, Rect::new(x, head.y, 22.0, head.h), &icons::ELLIPSIS, Hit::ExtTree(vi as u16, TreeHit::More), fg);
            x -= 24.0;
        }
        for (k, (_, _, _, icon, _, _)) in buttons.iter().enumerate().rev() {
            let icon = icon.unwrap_or(&icons::DOT);
            self.icon_button(c, Rect::new(x, head.y, 22.0, head.h), icon, Hit::ExtTree(vi as u16, TreeHit::Title(k as u8)), fg);
            x -= 24.0;
        }
    }

    /// `view/title` entries for view `vi` whose `when` holds: (buttons, the rest).
    fn ext_title_entries(&self, vi: usize) -> (Vec<MenuEntry>, Vec<MenuEntry>) {
        let view = &self.ext_views.list[vi];
        let ctx = self.when_context(&view.id, None);
        self.ext_views
            .title_menu
            .iter()
            .filter(|e| e.0 == view.ext && self.when_eval(e.5.as_deref(), &ctx))
            .cloned()
            .partition(|e| e.4.as_deref().is_some_and(|g| g.starts_with("navigation")) && e.3.is_some())
    }

    /// `view/item/context` entries for an item: (inline buttons, the context menu).
    fn ext_item_entries(&self, vi: usize, item: &TreeItem) -> (Vec<MenuEntry>, Vec<MenuEntry>) {
        let view = &self.ext_views.list[vi];
        let ctx = self.when_context(&view.id, Some(item));
        self.ext_views
            .item_menu
            .iter()
            .filter(|e| e.0 == view.ext && self.when_eval(e.5.as_deref(), &ctx))
            .cloned()
            .partition(|e| e.4.as_deref().is_some_and(|g| g.starts_with("inline")))
    }

    /// The rows shown: the roots and the children of expanded items. Asks for children that
    /// aren't loaded yet.
    pub(super) fn ext_tree_rows(&mut self, vi: usize) -> (Vec<Row>, Option<String>) {
        let view = self.ext_views.list[vi].id.clone();
        if !self.ext_views.state.get(&view).is_some_and(|s| s.children.contains_key("")) {
            self.ext_tree_request(vi, "");
        }
        let st = &self.ext_views.state[&view];
        let message = match st.children.get("") {
            Some(Children::Failed(e)) => Some(e.clone()),
            _ => None,
        };
        let mut rows = Vec::new();
        let mut missing = Vec::new();
        fn walk(st: &TreeState, parent: &str, depth: usize, rows: &mut Vec<Row>, missing: &mut Vec<String>) {
            let Some(Children::Items(items)) = st.children.get(parent) else { return };
            for item in items {
                let expanded = item.collapsible > 0 && (st.expanded.contains(&item.id) || (item.collapsible == 2 && !st.collapsed.contains(&item.id)));
                rows.push(Row { depth, item: item.clone(), expanded });
                if expanded {
                    if st.children.contains_key(&item.id) {
                        walk(st, &item.id, depth + 1, rows, missing);
                    } else {
                        missing.push(item.id.clone());
                    }
                }
            }
        }
        walk(st, "", 0, &mut rows, &mut missing);
        for id in missing {
            self.ext_tree_request(vi, &id);
        }
        (rows, message)
    }

    /// Asks view `vi`'s extension for `element`'s children ("" for the roots), starting it if
    /// needed (`onView:<id>`).
    fn ext_tree_request(&mut self, vi: usize, element: &str) {
        let view = self.ext_views.list[vi].clone();
        let st = self.ext_views.state.entry(view.id.clone()).or_default();
        if matches!(st.children.get(element), Some(Children::Loading)) {
            return;
        }
        let Some(e) = contributions::with(|r| r.get(&view.ext).cloned()) else { return };
        if e.program().is_none() {
            st.children.insert(element.to_string(), Children::Failed(NO_PROVIDER.into()));
            return;
        }
        st.children.insert(element.to_string(), Children::Loading);
        self.ext_start(&e, &format!("onView:{}", view.id));
        let params = json!({ "viewId": view.id, "element": if element.is_empty() { Value::Null } else { json!(element) } });
        self.ext_ask(&view.ext, "treeView/getChildren", params, super::ext_host::Waiter::Tree(view.id.clone(), element.to_string()));
    }

    /// The answer to `treeView/getChildren`.
    pub(super) fn ext_tree_answer(&mut self, view: &str, element: &str, result: Result<Value, String>) {
        let st = self.ext_views.state.entry(view.to_string()).or_default();
        let children = match result {
            Ok(Value::Array(items)) => Children::Items(items.iter().filter_map(TreeItem::parse).collect()),
            Ok(_) => Children::Items(Vec::new()),
            Err(e) if element.is_empty() && e.starts_with("no data for view") => Children::Failed(NO_PROVIDER.into()),
            Err(e) => Children::Failed(e),
        };
        st.children.insert(element.to_string(), children);
    }

    /// `treeView/refresh`: forget an item's children (or all), so they're asked for again.
    pub(super) fn ext_tree_refresh(&mut self, ext: &str, params: &Value) {
        let Some(view) = params["viewId"].as_str() else { return };
        if !self.ext_views.list.iter().any(|v| v.id == view && v.ext == ext) {
            return;
        }
        let st = self.ext_views.state.entry(view.to_string()).or_default();
        match params["element"].as_str() {
            Some(element) => {
                st.children.remove(element);
            }
            None => st.children.clear(),
        }
    }

    /// A tree item's icon (icon name, SVG or image in the extension).
    fn ext_item_icon(&mut self, vi: usize, icon: &str) -> Option<ItemIcon> {
        if let Some(name) = icon.strip_prefix("$(").and_then(|n| n.strip_suffix(')')) {
            return icons::named(name.split('~').next().unwrap_or(name)).map(ItemIcon::Vector);
        }
        let ext = self.ext_views.list[vi].ext.clone();
        let path = contributions::with(|r| r.get(&ext).map(|e| e.file(icon)))?;
        self.ext_views
            .images
            .entry(path.clone())
            .or_insert_with(|| {
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
                    return std::fs::read_to_string(&path).ok().and_then(|s| icons::from_svg(&s)).map(ItemIcon::Vector);
                }
                let bytes = std::fs::read(&path).ok()?;
                crate::imageio::decode(&bytes).ok().map(|i| ItemIcon::Image(Arc::new(i)))
            })
            .clone()
    }

    pub(super) fn draw_ext_tree(&mut self, c: &mut Canvas, r: Rect, vi: usize) {
        let (rows, message) = self.ext_tree_rows(vi);
        let fg = self.color_or("sideBar.foreground", "foreground");
        if let Some(msg) = message {
            let st = TextStyle::ui(UI, fg);
            for (i, line) in super::intel::wrap(c, &msg, &st, r.w - 40.0).iter().enumerate() {
                c.text(r.x + 20.0, r.y + 6.0 + i as f32 * 20.0, line, &st);
            }
            return;
        }
        let max = (rows.len() as f32 * ROW_H - r.h).max(0.0);
        let st = self.ext_tree_state(vi);
        st.scroll = st.scroll.clamp(0.0, max);
        let (scroll, selected) = (st.scroll, st.selected.clone());
        c.push_clip(r);
        let icon_fg = self.color("icon.foreground");
        for (i, row) in rows.iter().enumerate() {
            let rr = Rect::new(r.x, r.y + i as f32 * ROW_H - scroll, r.w, ROW_H);
            if rr.bottom() < r.y || rr.y > r.bottom() {
                continue;
            }
            let hit = Hit::ExtTree(vi as u16, TreeHit::Row(i as u32));
            let is_selected = selected.as_deref() == Some(row.item.id.as_str());
            let hovered = self.hover_hit.is_some_and(|h| matches!(h, Hit::ExtTree(v, TreeHit::Row(k) | TreeHit::Inline(k, _)) if v as usize == vi && k as usize == i));
            if is_selected {
                c.fill_rounded(super::row_pill(rr), self.color("list.inactiveSelectionBackground"), super::ROW_RADIUS);
            } else if hovered {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            self.hits.push((rr.intersect(&r), hit));
            let mut x = rr.x + 8.0 + row.depth as f32 * 8.0;
            if row.item.collapsible > 0 {
                let chevron = if row.expanded { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
                c.icon_in(chevron, Rect::new(x, rr.y, 16.0, ROW_H), 16.0, icon_fg);
            }
            x += 20.0;
            if let Some(icon) = row.item.icon.clone().and_then(|i| self.ext_item_icon(vi, &i)) {
                let ir = Rect::new(x, rr.y + 3.0, 16.0, 16.0);
                match icon {
                    ItemIcon::Vector(v) => c.icon_in(v, ir, 16.0, icon_fg),
                    ItemIcon::Image(img) => c.image(ir, &img),
                }
                x += 22.0;
            }
            // Inline actions on the hovered or selected row.
            let mut right = rr.right() - 6.0;
            if hovered || is_selected {
                let (inline, _) = self.ext_item_entries(vi, &row.item);
                for (k, entry) in inline.iter().enumerate().rev() {
                    right -= 22.0;
                    let ar = Rect::new(right, rr.y, 22.0, ROW_H);
                    self.icon_button(c, ar, entry.3.unwrap_or(&icons::DOT), Hit::ExtTree(vi as u16, TreeHit::Inline(i as u32, k as u8)), icon_fg);
                }
            }
            let label_st = TextStyle::ui(UI, fg);
            c.push_clip(Rect::new(x, rr.y, (right - x).max(0.0), ROW_H));
            let lw = c.text_in(Rect::new(x, rr.y, (right - x).max(0.0), ROW_H), &row.item.label, &label_st);
            if !row.item.description.is_empty() {
                let dim = TextStyle::ui(12.0, self.color("descriptionForeground"));
                c.text_in(Rect::new(x + lw + 6.0, rr.y, (right - x - lw - 6.0).max(0.0), ROW_H), &row.item.description, &dim);
            }
            c.pop_clip();
        }
        c.pop_clip();
    }

    pub(super) fn scroll_ext_tree(&mut self, vi: usize, dy: f32) {
        if vi < self.ext_views.list.len() {
            let st = self.ext_tree_state(vi);
            st.scroll = (st.scroll - dy).max(0.0);
        }
    }

    fn ext_row(&mut self, vi: usize, i: u32) -> Option<TreeItem> {
        let (rows, _) = self.ext_tree_rows(vi);
        rows.into_iter().nth(i as usize).map(|r| r.item)
    }

    pub(super) fn ext_tree_click(&mut self, vi: usize, hit: TreeHit, x: f32, y: f32) {
        if vi >= self.ext_views.list.len() {
            return;
        }
        match hit {
            TreeHit::Section => {
                let st = self.ext_tree_state(vi);
                st.open = !st.open;
            }
            TreeHit::Body => {}
            TreeHit::Row(i) => {
                let Some(item) = self.ext_row(vi, i) else { return };
                let (rows, _) = self.ext_tree_rows(vi);
                let expanded = rows.get(i as usize).is_some_and(|r| r.expanded);
                let st = self.ext_tree_state(vi);
                st.selected = Some(item.id.clone());
                if item.collapsible > 0 {
                    if expanded {
                        st.expanded.remove(&item.id);
                        st.collapsed.insert(item.id.clone());
                    } else {
                        st.expanded.insert(item.id.clone());
                        st.collapsed.remove(&item.id);
                    }
                }
                if let Some((command, args)) = item.command {
                    self.ext_execute(&command, args, None);
                }
            }
            TreeHit::Inline(i, k) => {
                let Some(item) = self.ext_row(vi, i) else { return };
                let (inline, _) = self.ext_item_entries(vi, &item);
                if let Some(entry) = inline.get(k as usize) {
                    self.ext_execute(&entry.1.clone(), vec![item.raw.clone()], None);
                }
            }
            TreeHit::Title(k) => {
                let (buttons, _) = self.ext_title_entries(vi);
                if let Some(entry) = buttons.get(k as usize) {
                    self.ext_execute(&entry.1.clone(), Vec::new(), None);
                }
            }
            TreeHit::More => {
                let (_, rest) = self.ext_title_entries(vi);
                let entries = rest.into_iter().map(|e| (PopupItem::Item { label: e.2, enabled: true, checked: None }, PopupAction::ExtCommand(e.1, Vec::new()))).collect();
                self.show_popup(entries, x, y);
            }
        }
    }

    /// Right-click on a row: its `view/item/context` menu.
    pub(super) fn ext_tree_context_menu(&mut self, vi: usize, i: u32, x: f32, y: f32) {
        if vi >= self.ext_views.list.len() {
            return;
        }
        let Some(item) = self.ext_row(vi, i) else { return };
        self.ext_tree_state(vi).selected = Some(item.id.clone());
        let (_, rest) = self.ext_item_entries(vi, &item);
        let mut entries: Vec<(PopupItem, PopupAction)> = Vec::new();
        let mut last_group: Option<Option<String>> = None;
        for e in rest {
            let group = e.4.clone().map(|g| g.split('@').next().unwrap_or("").to_string());
            if last_group.as_ref().is_some_and(|g| *g != group) {
                entries.push((PopupItem::Separator, PopupAction::None));
            }
            last_group = Some(group);
            entries.push((PopupItem::Item { label: e.2, enabled: true, checked: None }, PopupAction::ExtCommand(e.1, vec![item.raw.clone()])));
        }
        if !entries.is_empty() {
            self.show_popup(entries, x, y);
        }
    }
}
