//! What the enabled extensions contribute, gathered where the editor reads each kind of thing:
//! languages (at startup, for `language::init`), commands (`Command::Ext`), settings (the
//! settings schema), keybindings (under the user's), color themes, snippets, JSON schemas,
//! view containers (`View::Ext`, an index into a registry that never shrinks) and tree views.
//! The installed extensions live in `<user data>/extensions` (`extensions::Registry`).

use std::cell::RefCell;
use std::path::PathBuf;

use extensions::{Extension, Registry};
use serde_json::{json, Value};

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
    /// Bumped on every change to the registry (caches of what it contributes compare it).
    static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CONTAINERS: RefCell<Vec<Container>> = const { RefCell::new(Vec::new()) };
}

/// An activity bar view container from an extension (`View::Ext(i)` is `CONTAINERS[i]`).
#[derive(Clone)]
pub struct Container {
    pub id: String,
    pub title: &'static str,
    pub icon: &'static render::Icon,
}

/// A contributed tree view of an enabled extension.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub id: String,
    pub name: String,
    /// `explorer`, or an activity bar container's id.
    pub container: String,
    pub ext: String,
    pub when: Option<String>,
}

pub fn generation() -> u64 {
    GENERATION.with(|g| g.get())
}

/// The container registered as `View::Ext(i)`.
pub fn container(i: u16) -> Option<Container> {
    CONTAINERS.with(|c| c.borrow().get(i as usize).cloned())
}

pub fn container_index(id: &str) -> Option<u16> {
    CONTAINERS.with(|c| c.borrow().iter().position(|k| k.id == id).map(|i| i as u16))
}

/// An icon for an extension's view container or command: an icon name (`$(name)`), or an SVG
/// file in the extension (its paths; other images fall back to the extensions icon).
pub fn extension_icon(e: &Extension, icon: Option<&str>) -> &'static render::Icon {
    let Some(icon) = icon else { return &crate::icons::EXTENSIONS };
    if let Some(name) = icon.strip_prefix("$(").and_then(|n| n.strip_suffix(')')) {
        return crate::icons::named(name.split('~').next().unwrap_or(name)).unwrap_or(&crate::icons::EXTENSIONS);
    }
    std::fs::read_to_string(e.file(icon)).ok().and_then(|svg| crate::icons::from_svg(&svg)).unwrap_or(&crate::icons::EXTENSIONS)
}

fn register_containers(e: &Extension) {
    for c in e.view_containers() {
        if container_index(&c.id).is_some() {
            continue;
        }
        let icon = extension_icon(e, c.icon.as_deref());
        let title: &'static str = Box::leak(c.title.into_boxed_str());
        CONTAINERS.with(|k| k.borrow_mut().push(Container { id: c.id, title, icon }));
    }
}

/// The tree views of the enabled extensions.
pub fn views() -> Vec<View> {
    enabled()
        .iter()
        .flat_map(|e| e.views().into_iter().map(|v| View { id: v.id, name: v.name, container: v.container, ext: e.id.clone(), when: v.when }))
        .collect()
}

/// The entries of a `contributes.menus` menu (`view/title`) from the enabled extensions, with
/// each command's title and icon: (extension, item, title, icon).
pub fn menu(menu: &str) -> Vec<(String, extensions::MenuItemDef, String, Option<&'static render::Icon>)> {
    let mut out = Vec::new();
    for e in enabled() {
        let commands = e.commands();
        for item in e.menu(menu) {
            let def = commands.iter().find(|c| c.command == item.command);
            let title = def.map_or_else(|| item.command.clone(), |c| c.title.clone());
            let icon = def.and_then(|c| c.icon.as_deref()).map(|i| extension_icon(&e, Some(i)));
            out.push((e.id.clone(), item, title, icon));
        }
    }
    out
}

pub fn dir() -> PathBuf {
    settings::user_data_dir().join("extensions")
}

pub fn with<T>(f: impl FnOnce(&Registry) -> T) -> T {
    REGISTRY.with(|r| f(&r.borrow()))
}

pub fn with_mut<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    GENERATION.with(|g| g.set(g.get() + 1));
    REGISTRY.with(|r| f(&mut r.borrow_mut()))
}

/// The enabled extensions (copies, so callers can hold them across calls).
pub fn enabled() -> Vec<Extension> {
    with(|r| r.enabled().cloned().collect())
}

/// Reads the installed extensions and registers the enabled ones' commands and settings.
/// Called once at startup, before the languages are loaded.
pub fn load() {
    let registry = Registry::scan(&dir());
    REGISTRY.with(|r| *r.borrow_mut() = registry);
    GENERATION.with(|g| g.set(g.get() + 1));
    for e in enabled() {
        register(&e);
    }
}

/// Makes an extension's commands and settings known (after installing or enabling it too).
pub fn register(e: &Extension) {
    for c in e.commands() {
        let title = match &c.category {
            Some(cat) => format!("{cat}: {}", c.title),
            None => c.title.clone(),
        };
        crate::commands::register_ext_command(&c.command, Some(&title), &e.id);
    }
    crate::commands::set_ext_commands_enabled(&e.id, true);
    register_containers(e);
    settings::schema::extend(e.settings().iter().map(|s| settings::schema::extension_setting(&s.key, &s.schema, &s.section)).collect());
}

/// The contributed languages, as `languages.json` entries (the user's file goes after them).
pub fn languages() -> Vec<Value> {
    enabled().iter().flat_map(Extension::languages).collect()
}

/// The contributed color themes.
pub fn themes() -> Vec<theme::ThemeInfo> {
    let mut out: Vec<theme::ThemeInfo> = enabled()
        .iter()
        .flat_map(Extension::themes)
        .map(|t| theme::ThemeInfo { name: t.label, kind: theme::ThemeKind::from_type(&t.ui_theme), source: theme::ThemeSource::File(t.path) })
        .collect();
    out.sort_by_key(|t| t.name.to_lowercase());
    out
}

/// Snippet files for language `lang_id`: (file, whether it's global and filtered by `scope`).
pub fn snippet_files(lang_id: &str) -> Vec<(PathBuf, bool)> {
    enabled()
        .iter()
        .flat_map(Extension::snippets)
        .filter_map(|(lang, path)| match lang {
            Some(l) if l == lang_id => Some((path, false)),
            Some(_) => None,
            None => Some((path, true)),
        })
        .collect()
}

/// Contributed keybindings in `keybindings.json`'s format (applied before the user's).
pub fn keybindings() -> Vec<Value> {
    enabled().iter().flat_map(Extension::keybindings).map(|b| json!({ "key": b.key, "command": b.command, "args": b.args })).collect()
}

/// Contributed JSON schemas (`jsonValidation`) for the JSON server. Schemas that are files in
/// the extension are read; web addresses aren't fetched.
pub fn json_schemas() -> Vec<Value> {
    enabled()
        .iter()
        .flat_map(Extension::json_validation)
        .filter_map(|(patterns, url)| {
            if url.contains("://") {
                return None;
            }
            let schema = extensions::manifest::read_json(std::path::Path::new(&url)).ok()?;
            Some(json!({ "fileMatch": patterns, "uri": lsp::path_to_uri(std::path::Path::new(&url)), "schema": schema }))
        })
        .collect()
}
