//! Installed extensions: the folders in `<user data>/extensions` (one per extension, named
//! `publisher.name-version`), plus folders installed from where they are
//! (Install Extension from Location, for developing one). `.orbvane.json` there lists those
//! folders, the disabled extensions and the ones installed from a marketplace: Open VSX
//! (`gallery`) or Orbvane's own registry (`native`, see `catalog`).
//!
//! An extension is a `package.json` (see `manifest`), and optionally a program
//! the editor runs and talks to over JSON-RPC (see the `orbvane-extension` crate).

pub mod catalog;
pub mod gallery;
pub mod manifest;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

pub use manifest::{arch, CommandDef, Extension, KeybindingDef, MenuItemDef, SettingDef, ThemeDef, ViewContainerDef, ViewDef};

const STATE_FILE: &str = ".orbvane.json";

#[derive(Default)]
pub struct Registry {
    pub dir: PathBuf,
    /// Every installed extension, by display name.
    pub all: Vec<Extension>,
    /// Ids of disabled extensions.
    pub disabled: HashSet<String>,
    /// Folders installed from where they are.
    linked: Vec<PathBuf>,
    /// Ids of extensions installed from Open VSX (checked for updates there).
    pub gallery: HashSet<String>,
    /// Ids of extensions installed from Orbvane's registry (checked for updates there).
    pub native: HashSet<String>,
    /// The user accepted that registry extensions are programs that run with their permissions.
    pub native_trusted: bool,
    /// Extensions that couldn't be read.
    pub errors: Vec<String>,
}

/// `1.10.0` > `1.9.2`.
/// Whether version `a` is newer than `b` (`1.10.0` > `1.9.2`).
pub fn is_newer(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
}

impl Registry {
    /// Reads the extensions installed in `dir`.
    pub fn scan(dir: &Path) -> Registry {
        let mut r = Registry { dir: dir.to_path_buf(), ..Default::default() };
        if let Ok(state) = manifest::read_json(&dir.join(STATE_FILE)) {
            r.disabled = state["disabled"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
            r.gallery = state["gallery"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
            r.native_trusted = state["nativeTrusted"].as_bool().unwrap_or(false);
            r.native = state["native"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
            r.linked = state["linked"].as_array().into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from).collect();
        }
        let mut found: Vec<Extension> = Vec::new();
        for path in &r.linked {
            match Extension::load(path) {
                Ok(mut e) => {
                    e.linked = true;
                    found.push(e);
                }
                Err(err) => r.errors.push(err),
            }
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        entries.sort();
        for path in entries {
            if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            match Extension::load(&path) {
                Ok(e) => match found.iter().position(|f| f.id == e.id) {
                    // A linked folder wins; otherwise the newest version.
                    Some(i) if !found[i].linked && version_key(&e.version) > version_key(&found[i].version) => found[i] = e,
                    Some(_) => {}
                    None => found.push(e),
                },
                Err(err) => r.errors.push(err),
            }
        }
        found.sort_by_key(|e| e.display_name.to_lowercase());
        r.all = found;
        r
    }

    pub fn get(&self, id: &str) -> Option<&Extension> {
        self.all.iter().find(|e| e.id == id.to_lowercase())
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        !self.disabled.contains(&id.to_lowercase())
    }

    /// The installed extensions that are enabled.
    pub fn enabled(&self) -> impl Iterator<Item = &Extension> {
        self.all.iter().filter(|e| self.is_enabled(&e.id))
    }

    pub(crate) fn save_state(&self) -> Result<(), String> {
        let mut disabled: Vec<&String> = self.disabled.iter().collect();
        disabled.sort();
        let mut gallery: Vec<&String> = self.gallery.iter().collect();
        gallery.sort();
        let mut native: Vec<&String> = self.native.iter().collect();
        native.sort();
        let state = json!({ "linked": self.linked, "disabled": disabled, "gallery": gallery, "native": native, "nativeTrusted": self.native_trusted });
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join(STATE_FILE), serde_json::to_string_pretty(&state).unwrap()).map_err(|e| e.to_string())
    }

    /// Remembers that the user accepted the warning about registry extensions.
    pub fn trust_native(&mut self) -> Result<(), String> {
        self.native_trusted = true;
        self.save_state()
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        let id = id.to_lowercase();
        if enabled {
            self.disabled.remove(&id);
        } else {
            self.disabled.insert(id);
        }
        self.save_state()
    }

    /// Installs a `.vsix` package (a zip with the extension in `extension/`), replacing other
    /// versions of it. Returns its id.
    pub fn install_vsix(&mut self, vsix: &Path) -> Result<String, String> {
        self.install_package(vsix, None)
    }

    /// Installs a package downloaded from a marketplace, remembering which (for updates).
    pub fn install_from(&mut self, vsix: &Path, source: gallery::Source) -> Result<String, String> {
        self.install_package(vsix, Some(source))
    }

    fn install_package(&mut self, vsix: &Path, source: Option<gallery::Source>) -> Result<String, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let tmp = self.dir.join(format!(".install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let out = Command::new("/usr/bin/unzip")
            .args(["-q", "-o"])
            .arg(vsix)
            .args(["extension/*", "-d"])
            .arg(&tmp)
            .output()
            .map_err(|e| format!("Couldn't run unzip: {e}"))?;
        let result = (|| {
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(if err.is_empty() { format!("{} isn't a valid extension package", vsix.display()) } else { err });
            }
            let e = Extension::load(&tmp.join("extension")).map_err(|_| format!("{} has no extension/package.json", vsix.display()))?;
            self.gallery.remove(&e.id);
            self.native.remove(&e.id);
            match source {
                Some(gallery::Source::OpenVsx) => self.gallery.insert(e.id.clone()),
                Some(gallery::Source::Orbvane) => self.native.insert(e.id.clone()),
                None => false,
            };
            self.place(&tmp.join("extension"), &e)?;
            Ok(e.id)
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        let id = result?;
        *self = Registry::scan(&self.dir.clone());
        Ok(id)
    }

    /// Moves an unpacked extension into place as `publisher.name-version`, removing older copies.
    fn place(&mut self, from: &Path, e: &Extension) -> Result<(), String> {
        let target = self.dir.join(format!("{}-{}", e.id, e.version));
        for old in std::fs::read_dir(&self.dir).into_iter().flatten().flatten().map(|d| d.path()) {
            if old.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.')) && Extension::load(&old).is_ok_and(|o| o.id == e.id) {
                let _ = std::fs::remove_dir_all(&old);
            }
        }
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(from, &target).map_err(|err| format!("Couldn't install {}: {err}", e.display_name))?;
        self.linked.retain(|p| Extension::load(p).map_or(true, |l| l.id != e.id));
        self.save_state()
    }

    /// Installs the extension in `folder` from where it is (for developing it). Returns its id.
    pub fn install_folder(&mut self, folder: &Path) -> Result<String, String> {
        let e = Extension::load(folder)?;
        self.linked.retain(|p| p != folder && Extension::load(p).map_or(true, |l| l.id != e.id));
        self.linked.push(folder.to_path_buf());
        self.gallery.remove(&e.id);
        self.native.remove(&e.id);
        self.save_state()?;
        *self = Registry::scan(&self.dir.clone());
        Ok(e.id)
    }

    /// Removes an extension: deletes its copy, or forgets a folder installed from where it is.
    pub fn uninstall(&mut self, id: &str) -> Result<(), String> {
        let e = self.get(id).cloned().ok_or_else(|| format!("{id} isn't installed"))?;
        if e.linked {
            self.linked.retain(|p| p != &e.path);
        } else {
            std::fs::remove_dir_all(&e.path).map_err(|err| format!("Couldn't remove {}: {err}", e.path.display()))?;
        }
        self.disabled.remove(&e.id);
        self.gallery.remove(&e.id);
        self.native.remove(&e.id);
        self.save_state()?;
        *self = Registry::scan(&self.dir.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn installs_enables_and_uninstalls() {
        let root = std::env::temp_dir().join(format!("orbvane-ext-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("extensions");
        write(&dir, &[("a.one-1.0.0/package.json", r#"{ "name": "one", "publisher": "a", "version": "1.0.0", "displayName": "One" }"#)]);
        write(&dir, &[("a.one-1.2.0/package.json", r#"{ "name": "one", "publisher": "a", "version": "1.2.0", "displayName": "One" }"#)]);
        write(&dir, &[("broken/package.json", "{")]);
        let mut r = Registry::scan(&dir);
        assert_eq!(r.all.len(), 1);
        assert_eq!(r.get("A.One").unwrap().version, "1.2.0");
        assert_eq!(r.errors.len(), 1);

        r.set_enabled("a.one", false).unwrap();
        assert!(!Registry::scan(&dir).is_enabled("a.one"));
        r.set_enabled("a.one", true).unwrap();
        assert!(Registry::scan(&dir).is_enabled("a.one"));

        // A folder installed from where it is.
        let dev = root.join("dev");
        write(&dev, &[("package.json", r#"{ "name": "two", "publisher": "b", "version": "0.0.1", "displayName": "Two" }"#)]);
        assert_eq!(r.install_folder(&dev).unwrap(), "b.two");
        assert!(r.get("b.two").unwrap().linked);
        r.uninstall("b.two").unwrap();
        assert!(r.get("b.two").is_none() && dev.join("package.json").exists());

        // A .vsix package.
        let pkg = root.join("pkg");
        write(&pkg, &[("extension/package.json", r#"{ "name": "one", "publisher": "a", "version": "2.0.0", "displayName": "One" }"#), ("extension.vsixmanifest", "<x/>")]);
        let vsix = root.join("one.vsix");
        let ok = Command::new("/usr/bin/zip").current_dir(&pkg).args(["-q", "-r"]).arg(&vsix).args(["extension", "extension.vsixmanifest"]).status().unwrap();
        assert!(ok.success());
        assert_eq!(r.install_vsix(&vsix).unwrap(), "a.one");
        assert_eq!(r.get("a.one").unwrap().version, "2.0.0");
        assert!(!dir.join("a.one-1.0.0").exists() && dir.join("a.one-2.0.0/package.json").exists());
        assert!(r.install_vsix(&root.join("dev/package.json")).is_err());
        r.uninstall("a.one").unwrap();
        assert!(!dir.join("a.one-2.0.0").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
