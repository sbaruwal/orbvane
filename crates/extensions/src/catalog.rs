//! Orbvane's own extension registry: extensions written for Orbvane, built from source by the
//! registry's CI (`cargo orbvane registry`) and listed in one `index.json`. Open VSX extensions
//! still come from Open VSX (`gallery`); this is the other half.
//!
//! The index is `{ "version": 1, "extensions": [...] }`, each entry shaped like an Open VSX
//! extension (`namespace`, `name`, `version`, `displayName`, `description`, `categories`,
//! `keywords`, `files: { download, sha256, readme, manifest, icon }`, ...) so `GalleryExtension`
//! reads both; `files.sha256` is the hex digest itself. The whole index is fetched once and
//! searched here. `ORBVANE_REGISTRY_URL` points at another index (tests use a local one).

use serde_json::Value;

use crate::gallery::{self, GalleryExtension, Query, Sort, Source};

pub const INDEX_URL: &str = "https://raw.githubusercontent.com/sbaruwal/orbvane-extensions/main/index.json";

/// Where the index is.
pub fn index_url() -> String {
    std::env::var("ORBVANE_REGISTRY_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| INDEX_URL.to_string())
}

/// Reads an index.
pub fn parse(index: &Value) -> Result<Vec<GalleryExtension>, String> {
    let version = index["version"].as_u64().unwrap_or(1);
    if version > 1 {
        return Err("The extension registry needs a newer version of Orbvane.".into());
    }
    let list = index["extensions"].as_array().ok_or("The extension registry's index has no extensions")?;
    Ok(list
        .iter()
        .filter_map(GalleryExtension::parse)
        .map(|mut e| {
            e.source = Source::Orbvane;
            e.verified = false;
            e
        })
        .collect())
}

/// Downloads the index.
pub fn fetch(url: &str) -> Result<Vec<GalleryExtension>, String> {
    let bytes = gallery::get(url)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("The extension registry's index is damaged: {e}"))?;
    parse(&v)
}

/// The extensions matching `q`: every word in the name, id, description or keywords, in the
/// category if one is given. Sorted by name, or by `@sort:updated`; names matching first.
pub fn search(all: &[GalleryExtension], q: &Query) -> Vec<GalleryExtension> {
    let words: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
    let mut found: Vec<(bool, &GalleryExtension)> = all
        .iter()
        .filter(|e| q.category.as_ref().is_none_or(|c| e.categories.iter().any(|ec| ec.eq_ignore_ascii_case(c))))
        .filter_map(|e| {
            let name = format!("{} {}", e.display_name, e.id()).to_lowercase();
            let rest = format!("{} {} {}", e.description, e.keywords.join(" "), e.publisher).to_lowercase();
            words.iter().all(|w| name.contains(w) || rest.contains(w)).then(|| (words.iter().all(|w| name.contains(w)), e))
        })
        .collect();
    found.sort_by(|(an, a), (bn, b)| match q.sort {
        Sort::Updated => b.timestamp.cmp(&a.timestamp),
        _ => bn.cmp(an).then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase())),
    });
    found.into_iter().map(|(_, e)| e.clone()).collect()
}

/// The registry's entry for `id`.
pub fn find<'a>(all: &'a [GalleryExtension], id: &str) -> Option<&'a GalleryExtension> {
    all.iter().find(|e| e.id() == id.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery::testing::serve;
    use crate::Registry;

    fn entry(name: &str, description: &str, category: &str, timestamp: &str) -> Value {
        serde_json::json!({
            "namespace": "orbvane", "name": name, "version": "0.1.0", "displayName": name, "description": description,
            "categories": [category], "keywords": ["todo"], "timestamp": timestamp, "files": {}
        })
    }

    #[test]
    fn searches_the_index() {
        let index = serde_json::json!({ "version": 1, "extensions": [
            entry("todo-tree", "TODO comments in a tree", "Other", "2026-10-01"),
            entry("word-count", "Counts words", "Other", "2026-10-03"),
            entry("dark", "A dark theme with todo colors", "Themes", "2026-10-02"),
        ]});
        let all = parse(&index).unwrap();
        assert!(all.iter().all(|e| e.source == Source::Orbvane));
        let names = |q: &str| search(&all, &Query::parse(q)).iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["dark", "todo-tree", "word-count"]);
        // A name match comes before a description or keyword match.
        assert_eq!(names("todo"), ["todo-tree", "dark", "word-count"]);
        assert_eq!(names("count words"), ["word-count"]);
        assert_eq!(names("@category:themes"), ["dark"]);
        assert_eq!(names("@sort:updated"), ["word-count", "dark", "todo-tree"]);
        assert_eq!(find(&all, "Orbvane.Word-Count").unwrap().name, "word-count");
        assert!(parse(&serde_json::json!({ "version": 2, "extensions": [] })).is_err());
    }

    #[test]
    fn installs_from_the_index() {
        let root = std::env::temp_dir().join(format!("orbvane-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("extension")).unwrap();
        std::fs::write(pkg.join("extension/package.json"), r#"{ "name": "word-count", "publisher": "orbvane", "version": "0.1.0" }"#).unwrap();
        let vsix = root.join("w.vsix");
        assert!(std::process::Command::new("/usr/bin/zip").current_dir(&pkg).args(["-q", "-r"]).arg(&vsix).arg("extension").status().unwrap().success());
        let (bytes, sha) = (std::fs::read(&vsix).unwrap(), gallery::sha256_of(&vsix).unwrap());
        let base = serve(move |base| {
            let mut e = entry("word-count", "Counts words", "Other", "");
            e["files"] = serde_json::json!({ "download": format!("{base}/w.vsix"), "sha256": sha });
            vec![("/index.json".into(), serde_json::json!({ "extensions": [e] }).to_string().into_bytes()), ("/w.vsix".into(), bytes)]
        });
        let all = fetch(&format!("{base}/index.json")).unwrap();
        let mut reg = Registry::scan(&root.join("extensions"));
        let file = gallery::download_path(&root, &all[0]);
        gallery::download(&all[0], &file).unwrap();
        assert_eq!(reg.install_from(&file, Source::Orbvane).unwrap(), "orbvane.word-count");
        let reg = Registry::scan(&reg.dir);
        assert!(reg.native.contains("orbvane.word-count") && !reg.gallery.contains("orbvane.word-count"));

        // A checksum that doesn't match is refused.
        let mut bad = all[0].clone();
        bad.sha256 = Some("0".repeat(64));
        assert!(gallery::download(&bad, &file).unwrap_err().contains("damaged"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
