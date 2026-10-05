//! The marketplace side of the Extensions view: searches, popular extensions, icons, READMEs,
//! installs and update checks. Two marketplaces: Orbvane's own registry (`extensions::catalog`,
//! one index fetched and searched locally) for extensions written for Orbvane, and Open VSX
//! (`extensions::gallery`) for Open VSX extensions. Every request runs on its own thread and
//! answers through a channel (waking the UI); `marketplace_tick` applies the answers.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use extensions::catalog;
use extensions::gallery::{self, GalleryExtension, Page, Query, Source};
use render::Image;
use serde_json::Value;

use super::notifications::Severity;
use super::Workbench;

/// How long typing pauses before the marketplace is searched.
const SEARCH_DELAY: Duration = Duration::from_millis(300);
/// We check for updates every 12 hours.
const UPDATE_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// The first check waits for startup to settle.
const FIRST_CHECK: Duration = Duration::from_secs(5);
pub(super) const PAGE_SIZE: usize = 50;
/// Icons are fetched one at a time with this gap, to stay well inside the marketplace's free
/// request rate (a list opens with up to 50 of them).
const ICON_GAP: Duration = Duration::from_millis(400);

/// Installs write the extensions folder; one at a time.
static INSTALLING: Mutex<()> = Mutex::new(());

enum Reply {
    Search(u64, Result<Page, String>),
    Popular(Result<Page, String>),
    /// Orbvane's registry: every extension in it.
    Catalog(Result<Vec<GalleryExtension>, String>),
    Icon(String, Option<Vec<u8>>),
    /// An extension's page: its details, its README (or an error) and its package.json.
    Page(String, Option<GalleryExtension>, Result<String, String>, Option<Value>),
    /// (id, display name, an error, whether it was an update)
    Installed(String, String, Result<(), String>, bool),
    /// The newer versions of installed extensions; `manual` when the user asked.
    Updates(Result<Vec<GalleryExtension>, String>, bool),
}

/// What the view shows for a marketplace list.
pub(super) enum Results {
    Loading,
    Done(Page),
    Failed(String),
}

pub(super) struct Marketplace {
    /// Open VSX's address.
    service: String,
    /// Orbvane's registry index.
    pub index_url: String,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    /// The query the results are for, and the request that's current.
    pub query: Option<String>,
    seq: u64,
    typed: Option<Instant>,
    pub results: Option<Results>,
    pub popular: Option<Results>,
    /// Orbvane's registry (all of it).
    pub catalog: Option<Results>,
    /// Every extension seen in the marketplace, by id (for pages and installs).
    pub known: HashMap<String, GalleryExtension>,
    /// Icons by URL (None: loading or unreadable).
    icons: HashMap<String, Option<Arc<Image>>>,
    /// Icons waiting to be fetched, whether one is being fetched, and when the next may start.
    icon_queue: std::collections::VecDeque<String>,
    icon_busy: bool,
    icon_next: Instant,
    /// Pages: the README (or a message), and the package.json once fetched.
    pub manifests: HashMap<String, Value>,
    pub installing: HashSet<String>,
    /// Newer versions of installed extensions, by id.
    pub updates: HashMap<String, GalleryExtension>,
    checking: bool,
    next_check: Instant,
}

impl Default for Marketplace {
    fn default() -> Self {
        let (tx, rx) = channel();
        Marketplace {
            service: gallery::service_url(),
            index_url: catalog::index_url(),
            tx,
            rx,
            query: None,
            seq: 0,
            typed: None,
            results: None,
            popular: None,
            catalog: None,
            known: HashMap::new(),
            icons: HashMap::new(),
            icon_queue: std::collections::VecDeque::new(),
            icon_busy: false,
            icon_next: Instant::now(),
            manifests: HashMap::new(),
            installing: HashSet::new(),
            updates: HashMap::new(),
            checking: false,
            next_check: Instant::now() + FIRST_CHECK,
        }
    }
}

impl Workbench {
    fn gallery_spawn(&self, job: impl FnOnce() -> Reply + Send + 'static) {
        let (tx, waker) = (self.marketplace.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            waker();
        });
    }

    /// The marketplace search the Extensions view's search box asks for, if any (`@installed`
    /// and `@updates` filter the installed extensions instead; an empty box shows Popular).
    pub(super) fn marketplace_query(&self) -> Option<String> {
        let text = self.extensions.search.text.trim();
        let local = ["@installed", "@updates", "@outdated", "@enabled", "@disabled", "@builtin"];
        (!text.is_empty() && !text.split_whitespace().any(|w| local.contains(&w))).then(|| text.to_string())
    }

    /// The installed extensions' filter for `@installed`-style searches: (words, filter).
    pub(super) fn installed_filter(&self) -> (String, Option<&'static str>) {
        let mut filter = None;
        let mut words = Vec::new();
        for w in self.extensions.search.text.split_whitespace() {
            match w {
                "@installed" | "@builtin" => filter = filter.or(Some("installed")),
                "@updates" | "@outdated" => filter = Some("updates"),
                "@enabled" => filter = Some("enabled"),
                "@disabled" => filter = Some("disabled"),
                w if !w.starts_with('@') => words.push(w),
                _ => {}
            }
        }
        (words.join(" ").to_lowercase(), filter)
    }

    /// Fetches Orbvane's registry, the first time the view needs it.
    pub(super) fn ensure_catalog(&mut self) {
        if self.marketplace.catalog.is_none() {
            self.marketplace.catalog = Some(Results::Loading);
            let url = self.marketplace.index_url.clone();
            self.gallery_spawn(move || Reply::Catalog(catalog::fetch(&url)));
        }
    }

    /// Orbvane's registry's extensions matching `query` (all of them for an empty one).
    pub(super) fn catalog_results(&self, query: &str) -> Option<Results> {
        Some(match self.marketplace.catalog.as_ref()? {
            Results::Loading => Results::Loading,
            Results::Failed(e) => Results::Failed(e.clone()),
            Results::Done(page) => {
                let found = catalog::search(&page.extensions, &Query::parse(query));
                Results::Done(Page { total: found.len() as u64, extensions: found })
            }
        })
    }

    /// Asks for the popular extensions, the first time the view needs them.
    pub(super) fn ensure_popular(&mut self) {
        if self.marketplace.popular.is_none() {
            self.marketplace.popular = Some(Results::Loading);
            let base = self.marketplace.service.clone();
            self.gallery_spawn(move || Reply::Popular(gallery::search(&base, &Query::parse("@sort:installs"), 0, PAGE_SIZE)));
        }
    }

    /// Searches again (the view's Refresh, or after a failure).
    pub(super) fn refresh_marketplace(&mut self) {
        self.marketplace.popular = None;
        self.marketplace.catalog = None;
        self.marketplace.query = None;
        self.marketplace.typed = Some(Instant::now() - SEARCH_DELAY);
    }

    pub(super) fn marketplace_deadline(&self) -> Option<Instant> {
        let m = &self.marketplace;
        let typed = m.typed.map(|t| t + SEARCH_DELAY);
        let check = (crate::contributions::with(|r| !r.gallery.is_empty() || !r.native.is_empty()) && self.settings.bool("extensions.autoCheckUpdates")).then_some(m.next_check);
        let icon = (!m.icon_busy && !m.icon_queue.is_empty()).then_some(m.icon_next);
        [typed, check, icon].into_iter().flatten().min()
    }

    pub(super) fn marketplace_tick(&mut self) {
        // Search when typing pauses.
        let want = self.marketplace_query();
        if want.is_none() {
            self.marketplace.typed = None;
        } else if want != self.marketplace.query {
            match self.marketplace.typed {
                None => self.marketplace.typed = Some(Instant::now()),
                Some(t) if t.elapsed() >= SEARCH_DELAY => {
                    self.marketplace.typed = None;
                    self.marketplace.query = want.clone();
                    self.marketplace.seq += 1;
                    self.marketplace.results = Some(Results::Loading);
                    let (seq, base, q) = (self.marketplace.seq, self.marketplace.service.clone(), Query::parse(&want.unwrap_or_default()));
                    self.gallery_spawn(move || Reply::Search(seq, gallery::search(&base, &q, 0, PAGE_SIZE)));
                }
                Some(_) => {}
            }
        }
        if Instant::now() >= self.marketplace.next_check {
            self.marketplace.next_check = Instant::now() + UPDATE_INTERVAL;
            if self.settings.bool("extensions.autoCheckUpdates") {
                self.check_extension_updates(false);
            }
        }
        while let Ok(reply) = self.marketplace.rx.try_recv() {
            self.marketplace_reply(reply);
        }
        self.next_icon();
    }

    /// Starts fetching the next queued icon when it's time.
    fn next_icon(&mut self) {
        let m = &mut self.marketplace;
        if m.icon_busy || Instant::now() < m.icon_next {
            return;
        }
        let Some(url) = m.icon_queue.pop_front() else { return };
        m.icon_busy = true;
        m.icon_next = Instant::now() + ICON_GAP;
        self.gallery_spawn(move || {
            let bytes = gallery::get(&url).ok();
            Reply::Icon(url, bytes)
        });
    }

    /// Keeps what a marketplace said about its extensions. An id in both is Orbvane's.
    fn remember(&mut self, page: &Page) {
        for e in &page.extensions {
            match self.marketplace.known.get(&e.id()) {
                Some(k) if k.source == Source::Orbvane && e.source == Source::OpenVsx => {}
                _ => {
                    self.marketplace.known.insert(e.id(), e.clone());
                }
            }
        }
    }

    fn marketplace_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Search(seq, result) if seq == self.marketplace.seq => {
                if let Ok(page) = &result {
                    self.remember(page);
                }
                self.marketplace.results = Some(match result {
                    Ok(page) => Results::Done(page),
                    Err(e) => Results::Failed(e),
                });
            }
            Reply::Search(..) => {}
            Reply::Popular(result) => {
                if let Ok(page) = &result {
                    self.remember(page);
                }
                self.marketplace.popular = Some(match result {
                    Ok(page) => Results::Done(page),
                    Err(e) => Results::Failed(e),
                });
            }
            Reply::Catalog(result) => {
                self.marketplace.catalog = Some(match result {
                    Ok(list) => {
                        let page = Page { total: list.len() as u64, extensions: list };
                        self.remember(&page);
                        Results::Done(page)
                    }
                    Err(e) => Results::Failed(e),
                });
            }
            Reply::Icon(url, bytes) => {
                self.marketplace.icon_busy = false;
                let image = bytes.and_then(|b| crate::imageio::decode(&b).ok()).map(Arc::new);
                self.marketplace.icons.insert(url, image);
            }
            Reply::Page(id, details, readme, manifest) => {
                if let Some(d) = details {
                    self.marketplace.known.insert(id.clone(), d);
                }
                if let Some(m) = manifest {
                    self.marketplace.manifests.insert(id.clone(), m);
                }
                let text = readme.unwrap_or_else(|e| format!("Couldn't load the README: {e}"));
                self.set_extension_page_text(&id, &text);
            }
            Reply::Installed(id, name, result, update) => {
                self.marketplace.installing.remove(&id);
                match result {
                    Ok(()) => {
                        self.marketplace.updates.remove(&id);
                        let dir = crate::contributions::dir();
                        crate::contributions::with_mut(|r| *r = extensions::Registry::scan(&dir));
                        self.extension_installed(Ok(id), None);
                    }
                    Err(e) => {
                        let verb = if update { "updating" } else { "installing" };
                        self.notify(Severity::Error, &format!("Error while {verb} '{name}' extension. {e}"), "Extensions", Vec::new(), None);
                    }
                }
            }
            Reply::Updates(result, manual) => {
                self.marketplace.checking = false;
                let found = match result {
                    Ok(found) => found,
                    Err(e) => {
                        if manual {
                            self.notify(Severity::Error, &format!("Couldn't check for extension updates: {e}"), "Extensions", Vec::new(), None);
                        }
                        return;
                    }
                };
                self.marketplace.updates = found.into_iter().map(|e| (e.id(), e)).collect();
                if self.marketplace.updates.is_empty() {
                    if manual {
                        self.notify(Severity::Info, "All extensions are up to date.", "Extensions", Vec::new(), None);
                    }
                } else if self.settings.bool("extensions.autoUpdate") {
                    self.update_all_extensions();
                }
            }
        }
    }

    /// A marketplace icon, fetched the first time it's asked for.
    pub(super) fn gallery_icon(&mut self, url: &str) -> Option<Arc<Image>> {
        if let Some(icon) = self.marketplace.icons.get(url) {
            return icon.clone();
        }
        self.marketplace.icons.insert(url.to_string(), None);
        self.marketplace.icon_queue.push_back(url.to_string());
        self.next_icon();
        None
    }

    /// Fetches what an extension's marketplace page shows: its details (search results leave
    /// out the README and reviews), README and package.json.
    pub(super) fn fetch_extension_page(&mut self, e: &GalleryExtension) {
        let (id, base) = (e.id(), self.marketplace.service.clone());
        let (namespace, name) = (e.namespace.clone(), e.name.clone());
        let native = (e.source == Source::Orbvane).then(|| e.clone());
        self.gallery_spawn(move || {
            // Orbvane's registry's entries are complete already.
            let e = match native.map_or_else(|| gallery::details(&base, &namespace, &name), Ok) {
                Ok(e) => e,
                Err(err) => return Reply::Page(id, None, Err(err), None),
            };
            let readme = match &e.readme {
                Some(url) => gallery::get_text(url),
                None => Ok("No README available.".to_string()),
            };
            let manifest = e.manifest.as_deref().and_then(|u| gallery::get(u).ok()).and_then(|b| serde_json::from_slice(&b).ok());
            Reply::Page(id, Some(e), readme, manifest)
        });
    }

    /// Downloads and installs an extension from its marketplace (the latest version; from Open
    /// VSX, the build for this Mac when there's such a build).
    pub(super) fn install_from_marketplace(&mut self, id: &str, update: bool) {
        let Some(e) = self.marketplace.updates.get(id).or_else(|| self.marketplace.known.get(id)).cloned() else { return };
        if e.source == Source::Orbvane && !update && !self.accept_native_extensions(&e.display_name) {
            return;
        }
        if !self.marketplace.installing.insert(id.to_string()) {
            return;
        }
        let (dir, id, base, index) = (crate::contributions::dir(), id.to_string(), self.marketplace.service.clone(), self.marketplace.index_url.clone());
        self.gallery_spawn(move || {
            let name = e.display_name.clone();
            let result = (|| {
                let latest = match e.source {
                    Source::OpenVsx => gallery::details(&base, &e.namespace, &e.name)?,
                    Source::Orbvane => catalog::find(&catalog::fetch(&index)?, &e.id()).cloned().ok_or_else(|| format!("{} isn't in the registry anymore", e.display_name))?,
                };
                std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
                let file = gallery::download_path(&dir, &latest);
                gallery::download(&latest, &file)?;
                let _lock = INSTALLING.lock().unwrap_or_else(|p| p.into_inner());
                let result = extensions::Registry::scan(&dir).install_from(&file, latest.source);
                let _ = std::fs::remove_file(&file);
                result.map(|_| ())
            })();
            Reply::Installed(id, name, result, update)
        });
    }

    /// Before the first install from Orbvane's registry: its extensions are programs that run
    /// with the user's permissions. Asks once (in a window; tests have none).
    fn accept_native_extensions(&mut self, name: &str) -> bool {
        if self.window.is_none() || crate::contributions::with(|r| r.native_trusted) {
            return true;
        }
        let answer = self
            .message_dialog()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Install Extension")
            .set_description(format!(
                "Extensions from Orbvane's registry are programs that run on your Mac with your permissions, built from their published source code.\n\nInstall '{name}' only if you trust its publisher."
            ))
            .set_buttons(rfd::MessageButtons::OkCancelCustom("Install".into(), "Cancel".into()))
            .show();
        let ok = matches!(answer, rfd::MessageDialogResult::Custom(ref b) if b == "Install") || matches!(answer, rfd::MessageDialogResult::Ok);
        if ok {
            let _ = crate::contributions::with_mut(|r| r.trust_native());
        }
        ok
    }

    /// Extensions: Check for Extension Updates (`manual`), or the periodic check.
    pub(super) fn check_extension_updates(&mut self, manual: bool) {
        let installed: Vec<(String, String, Source)> = crate::contributions::with(|r| {
            r.all
                .iter()
                .filter(|e| !e.linked)
                .filter_map(|e| {
                    let source = if r.native.contains(&e.id) { Source::Orbvane } else if r.gallery.contains(&e.id) { Source::OpenVsx } else { return None };
                    Some((e.id.clone(), e.version.clone(), source))
                })
                .collect()
        });
        if installed.is_empty() {
            if manual {
                self.notify(Severity::Info, "All extensions are up to date.", "Extensions", Vec::new(), None);
            }
            return;
        }
        if self.marketplace.checking {
            return;
        }
        self.marketplace.checking = true;
        self.marketplace.next_check = Instant::now() + UPDATE_INTERVAL;
        let (base, index) = (self.marketplace.service.clone(), self.marketplace.index_url.clone());
        self.gallery_spawn(move || {
            let mut found = Vec::new();
            let mut failed = None;
            // Orbvane's registry: one index for all of them.
            let native = installed.iter().any(|(_, _, s)| *s == Source::Orbvane).then(|| catalog::fetch(&index));
            for (id, version, source) in installed {
                let Some((ns, name)) = id.split_once('.') else { continue };
                let latest = match (source, &native) {
                    (Source::Orbvane, Some(Ok(list))) => catalog::find(list, &id).cloned().ok_or_else(|| format!("{id} isn't in the registry anymore")),
                    (Source::Orbvane, Some(Err(e))) => Err(e.clone()),
                    _ => gallery::details(&base, ns, name),
                };
                match latest {
                    Ok(latest) if extensions::is_newer(&latest.version, &version) => found.push(latest),
                    Ok(_) => {}
                    Err(e) => failed = Some(e),
                }
            }
            Reply::Updates(if found.is_empty() { failed.map_or(Ok(found), Err) } else { Ok(found) }, manual)
        });
    }

    /// Extensions: Update All Extensions.
    pub(super) fn update_all_extensions(&mut self) {
        let ids: Vec<String> = self.marketplace.updates.keys().cloned().collect();
        for id in ids {
            self.install_from_marketplace(&id, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbench::extensions_view::ExtHit;
    use extensions::gallery::testing::serve;
    use std::process::Command;

    fn wait_for(wb: &mut Workbench, what: &str, done: impl Fn(&Workbench) -> bool) {
        let start = Instant::now();
        while !done(wb) {
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {what}: toasts {:?}", wb.toast_list());
            std::thread::sleep(Duration::from_millis(10));
            wb.marketplace_tick();
        }
    }

    /// A registry serving version `version` of a theme extension, acme.theme-x.
    fn registry(dir: &std::path::Path, version: &str) -> String {
        let pkg = dir.join(format!("pkg-{version}"));
        std::fs::create_dir_all(pkg.join("extension")).unwrap();
        let manifest = format!(r#"{{ "name": "theme-x", "publisher": "acme", "version": "{version}", "displayName": "Theme X", "description": "A theme." }}"#);
        std::fs::write(pkg.join("extension/package.json"), &manifest).unwrap();
        let vsix = dir.join(format!("x-{version}.vsix"));
        assert!(Command::new("/usr/bin/zip").current_dir(&pkg).args(["-q", "-r"]).arg(&vsix).arg("extension").status().unwrap().success());
        let bytes = std::fs::read(&vsix).unwrap();
        let sha = gallery::sha256_of(&vsix).unwrap();
        let version = version.to_string();
        serve(move |base| {
            let entry = serde_json::json!({
                "namespace": "acme", "name": "theme-x", "version": version, "displayName": "Theme X",
                "description": "A theme.", "downloadCount": 1500000, "averageRating": 4.0, "verified": true,
                "files": { "download": format!("{base}/x.vsix"), "sha256": format!("{base}/x.sha256"), "manifest": format!("{base}/package.json") }
            });
            vec![
                (gallery::search_url("", &Query::parse("theme"), 0, PAGE_SIZE), serde_json::json!({ "totalSize": 1, "extensions": [entry] }).to_string().into_bytes()),
                ("/api/acme/theme-x".to_string(), entry.to_string().into_bytes()),
                ("/x.vsix".to_string(), bytes),
                ("/x.sha256".to_string(), sha.into_bytes()),
                ("/package.json".to_string(), manifest.into_bytes()),
            ]
        })
    }

    /// Orbvane's registry serving version `version` of a theme, me.dusk.
    fn native_registry(dir: &std::path::Path, version: &str) -> String {
        let pkg = dir.join(format!("dusk-{version}"));
        std::fs::create_dir_all(pkg.join("extension")).unwrap();
        std::fs::write(pkg.join("extension/package.json"), format!(r#"{{ "name": "dusk", "publisher": "me", "version": "{version}", "engines": {{ "orbvane": "^0.1.0" }} }}"#)).unwrap();
        let vsix = dir.join(format!("dusk-{version}.vsix"));
        assert!(Command::new("/usr/bin/zip").current_dir(&pkg).args(["-q", "-r"]).arg(&vsix).arg("extension").status().unwrap().success());
        let (bytes, sha, version) = (std::fs::read(&vsix).unwrap(), gallery::sha256_of(&vsix).unwrap(), version.to_string());
        serve(move |base| {
            let entry = serde_json::json!({
                "namespace": "me", "name": "dusk", "version": version, "displayName": "Dusk", "description": "A dusky theme.",
                "categories": ["Themes"], "files": { "download": format!("{base}/dusk.vsix"), "sha256": sha }
            });
            vec![("/index.json".to_string(), serde_json::json!({ "version": 1, "extensions": [entry] }).to_string().into_bytes()), ("/dusk.vsix".to_string(), bytes)]
        })
    }

    /// Searching both marketplaces, installing from the results, and updating.
    #[test]
    fn searches_installs_and_updates() {
        let dir = std::env::temp_dir().join(format!("orbvane-marketplace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(None, &[], Arc::new(|| {}));
        let _ = wb.settings.set(settings::Scope::User, "extensions.autoUpdate", Some(serde_json::json!(false)));
        let _ = crate::contributions::with_mut(|r| {
            *r = extensions::Registry::scan(&crate::contributions::dir());
            r.uninstall("acme.theme-x")
        });
        wb.marketplace.service = registry(&dir, "1.0.0");
        wb.marketplace.index_url = format!("{}/index.json", native_registry(&dir, "0.1.0"));
        wb.marketplace.catalog = None;
        wb.ensure_catalog();

        // Typing searches once typing pauses: Orbvane's registry, then Open VSX.
        wb.extensions.search.set_text("theme");
        wait_for(&mut wb, "the results", |wb| matches!(wb.marketplace.results, Some(Results::Done(_))) && matches!(wb.marketplace.catalog, Some(Results::Done(_))));
        let sections = wb.extension_sections();
        assert_eq!(sections.len(), 2);
        // "A dusky theme." matches too.
        assert_eq!(sections[0].items.iter().map(|i| i.id()).collect::<Vec<_>>(), ["me.dusk"]);
        assert_eq!(sections[1].items.iter().map(|i| i.id()).collect::<Vec<_>>(), ["acme.theme-x"]);

        // Its page fetches the manifest; Install installs it (rows count across sections).
        wb.extensions_click(ExtHit::Row(1), 0.0, 0.0);
        wait_for(&mut wb, "the page", |wb| wb.marketplace.manifests.contains_key("acme.theme-x"));
        wb.extensions_click(ExtHit::Install(1), 0.0, 0.0);
        assert!(wb.marketplace.installing.contains("acme.theme-x"));
        wait_for(&mut wb, "the install", |wb| wb.marketplace.installing.is_empty());
        let installed = crate::contributions::with(|r| r.get("acme.theme-x").map(|e| e.version.clone()));
        assert_eq!(installed.as_deref(), Some("1.0.0"), "{:?}", wb.toast_list());
        assert!(crate::contributions::with(|r| r.gallery.contains("acme.theme-x")));

        // A newer version: found by the check, installed by Update.
        wb.marketplace.service = registry(&dir, "1.1.0");
        wb.check_extension_updates(true);
        wait_for(&mut wb, "the update check", |wb| !wb.marketplace.checking);
        assert_eq!(wb.marketplace.updates.get("acme.theme-x").map(|e| e.version.as_str()), Some("1.1.0"));
        wb.extensions.search.set_text("@updates");
        assert_eq!(wb.extension_sections()[0].items.len(), 1);
        wb.update_all_extensions();
        wait_for(&mut wb, "the update", |wb| wb.marketplace.installing.is_empty());
        assert_eq!(crate::contributions::with(|r| r.get("acme.theme-x").map(|e| e.version.clone())).as_deref(), Some("1.1.0"));
        assert!(wb.marketplace.updates.is_empty());

        wb.uninstall_extension("acme.theme-x");
        assert!(crate::contributions::with(|r| r.get("acme.theme-x").is_none() && !r.gallery.contains("acme.theme-x")));

        // Orbvane's registry: found by name, installed, updated from a newer index.
        wb.extensions.search.set_text("dusk");
        // (The stand-in Open VSX has nothing for it.)
        wait_for(&mut wb, "the search", |wb| wb.marketplace.query.as_deref() == Some("dusk") && !matches!(wb.marketplace.results, Some(Results::Loading)));
        assert_eq!(wb.extension_sections()[0].items.iter().map(|i| i.id()).collect::<Vec<_>>(), ["me.dusk"]);
        wb.extensions_click(ExtHit::Install(0), 0.0, 0.0);
        wait_for(&mut wb, "the install", |wb| wb.marketplace.installing.is_empty());
        assert!(crate::contributions::with(|r| r.native.contains("me.dusk") && !r.gallery.contains("me.dusk")), "{:?}", wb.toast_list());
        wb.marketplace.index_url = format!("{}/index.json", native_registry(&dir, "0.2.0"));
        wb.check_extension_updates(true);
        wait_for(&mut wb, "the update check", |wb| !wb.marketplace.checking);
        assert_eq!(wb.marketplace.updates.get("me.dusk").map(|e| (e.version.as_str(), e.source)), Some(("0.2.0", Source::Orbvane)));
        wb.update_all_extensions();
        wait_for(&mut wb, "the update", |wb| wb.marketplace.installing.is_empty());
        assert_eq!(crate::contributions::with(|r| r.get("me.dusk").map(|e| e.version.clone())).as_deref(), Some("0.2.0"));
        wb.uninstall_extension("me.dusk");
        let _ = wb.settings.set(settings::Scope::User, "extensions.autoUpdate", None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
