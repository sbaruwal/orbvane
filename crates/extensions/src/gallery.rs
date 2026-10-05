//! The marketplace: Open VSX (open-vsx.org), the open registry of Open VSX extensions, through
//! its REST API. Requests go through `/usr/bin/curl` (like git, a CLI that ships with macOS), so
//! the editor needs no TLS stack. Everything here blocks; the editor calls it from threads.
//! `ORBVANE_GALLERY_URL` points it at another server (tests use a local one).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::Registry;

pub const OPEN_VSX: &str = "https://open-vsx.org";

/// The registry's address.
pub fn service_url() -> String {
    std::env::var("ORBVANE_GALLERY_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| OPEN_VSX.to_string())
}

/// Which marketplace an extension comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Source {
    /// Open VSX: Open VSX extensions (their `package.json` contributions work, their JavaScript
    /// doesn't run).
    #[default]
    OpenVsx,
    /// Orbvane's own registry (`catalog`): extensions written for Orbvane.
    Orbvane,
}

/// An extension as a marketplace describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GalleryExtension {
    pub source: Source,
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub description: String,
    /// The publisher's name to show.
    pub publisher: String,
    pub downloads: u64,
    pub rating: Option<f64>,
    pub reviews: u64,
    /// The publisher owns the namespace.
    pub verified: bool,
    pub deprecated: bool,
    pub icon: Option<String>,
    pub download: Option<String>,
    /// The package's SHA-256: a URL to a file holding it (Open VSX), or the hex digest itself.
    pub sha256: Option<String>,
    pub readme: Option<String>,
    pub changelog: Option<String>,
    pub manifest: Option<String>,
    pub categories: Vec<String>,
    /// Search words (Open VSX's `tags`, `package.json`'s `keywords`).
    pub keywords: Vec<String>,
    pub repository: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    /// When this version was published (ISO 8601).
    pub timestamp: String,
    pub target_platform: String,
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

fn opt(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(String::from)
}

impl GalleryExtension {
    pub fn parse(v: &Value) -> Option<GalleryExtension> {
        let files = &v["files"];
        let name = text(v, "name");
        let namespace = text(v, "namespace");
        if name.is_empty() || namespace.is_empty() {
            return None;
        }
        Some(GalleryExtension {
            display_name: opt(v, "displayName").unwrap_or_else(|| name.clone()),
            publisher: opt(v, "namespaceDisplayName").or_else(|| v["publishedBy"]["loginName"].as_str().map(String::from)).unwrap_or_else(|| namespace.clone()),
            version: text(v, "version"),
            description: text(v, "description"),
            downloads: v["downloadCount"].as_u64().unwrap_or(0),
            rating: v["averageRating"].as_f64(),
            reviews: v["reviewCount"].as_u64().unwrap_or(0),
            verified: v["verified"].as_bool().unwrap_or(false),
            deprecated: v["deprecated"].as_bool().unwrap_or(false),
            icon: opt(files, "icon"),
            download: opt(files, "download"),
            sha256: opt(files, "sha256"),
            readme: opt(files, "readme"),
            changelog: opt(files, "changelog"),
            manifest: opt(files, "manifest"),
            categories: v["categories"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(),
            keywords: v["tags"].as_array().or(v["keywords"].as_array()).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(),
            repository: opt(v, "repository").map(|r| r.trim_start_matches("git+").trim_end_matches(".git").to_string()),
            homepage: opt(v, "homepage"),
            license: opt(v, "license"),
            timestamp: text(v, "timestamp"),
            target_platform: opt(v, "targetPlatform").unwrap_or_else(|| "universal".into()),
            source: Source::OpenVsx,
            namespace,
            name,
        })
    }

    /// `namespace.name`, lowercase, like installed extensions' ids.
    pub fn id(&self) -> String {
        format!("{}.{}", self.namespace, self.name).to_lowercase()
    }
}

/// Whether an extension with this manifest has JavaScript code (which Orbvane doesn't run):
/// its extensions are written for another editor, so any `main` is JavaScript unless `orbvane.main` is set.
pub fn runs_javascript(manifest: &Value) -> bool {
    manifest["orbvane"]["main"].is_null() && (manifest["main"].is_string() || manifest["browser"].is_string())
}

/// A page of search results.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub total: u64,
    pub extensions: Vec<GalleryExtension>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Relevance,
    Installs,
    Rating,
    Updated,
}

impl Sort {
    /// From `@sort:installs` (names; `name` isn't offered by Open VSX).
    pub fn parse(s: &str) -> Option<Sort> {
        Some(match s {
            "installs" | "downloads" => Sort::Installs,
            "rating" => Sort::Rating,
            "updateDate" | "publishedDate" | "updated" => Sort::Updated,
            "relevance" => Sort::Relevance,
            _ => return None,
        })
    }

    fn api(self) -> &'static str {
        match self {
            Sort::Relevance => "relevance",
            Sort::Installs => "downloadCount",
            Sort::Rating => "averageRating",
            Sort::Updated => "timestamp",
        }
    }
}

/// What to search for: words, and `@category:` and `@sort:` filters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub text: String,
    pub category: Option<String>,
    pub sort: Sort,
}

impl Query {
    /// Parses the Extensions view's search text (`@installed` and such are handled by the view).
    pub fn parse(input: &str) -> Query {
        let mut q = Query::default();
        let mut words = Vec::new();
        for word in shell_words(input) {
            if let Some(c) = word.strip_prefix("@category:") {
                q.category = Some(c.to_string());
            } else if let Some(s) = word.strip_prefix("@sort:") {
                q.sort = Sort::parse(s).unwrap_or_default();
            } else if word == "@popular" {
                q.sort = Sort::Installs;
            } else if !word.starts_with('@') {
                words.push(word);
            }
        }
        q.text = words.join(" ");
        q
    }
}

/// Words, with "quoted phrases" kept together (`@category:"programming languages"`).
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn search_url(base: &str, q: &Query, offset: usize, size: usize) -> String {
    let mut url = format!("{base}/api/-/search?size={size}&offset={offset}&includeAllVersions=false");
    if !q.text.is_empty() {
        url.push_str(&format!("&query={}", encode(&q.text)));
    }
    if let Some(c) = &q.category {
        url.push_str(&format!("&category={}", encode(c)));
    }
    // Without words, relevance means nothing: the most installed first.
    let sort = if q.sort == Sort::Relevance && q.text.is_empty() { Sort::Installs } else { q.sort };
    url.push_str(&format!("&sortBy={}&sortOrder=desc", sort.api()));
    url
}

/// `GET url`, following redirects.
pub fn get(url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("/usr/bin/curl")
        .args(["-sSfL", "--max-time", "60", "-H", "Accept: application/json", "-A", concat!("Orbvane/", env!("CARGO_PKG_VERSION"))])
        .arg(url)
        .output()
        .map_err(|e| format!("Couldn't run curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string();
        return Err(if err.is_empty() { format!("{url}: request failed") } else { err });
    }
    Ok(out.stdout)
}

fn get_json(url: &str) -> Result<Value, String> {
    let bytes = get(url)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{url}: {e}"))?;
    if let Some(err) = v["error"].as_str() {
        return Err(err.to_string());
    }
    Ok(v)
}

pub fn get_text(url: &str) -> Result<String, String> {
    get(url).map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub fn search(base: &str, q: &Query, offset: usize, size: usize) -> Result<Page, String> {
    let v = get_json(&search_url(base, q, offset, size))?;
    Ok(Page {
        total: v["totalSize"].as_u64().unwrap_or(0),
        extensions: v["extensions"].as_array().into_iter().flatten().filter_map(GalleryExtension::parse).collect(),
    })
}

/// This Mac's platform, as the registry names it.
pub fn target_platform() -> String {
    format!("darwin-{}", crate::arch())
}

/// The latest version of `namespace.name`: the build for this Mac if there is one, else the
/// universal one.
pub fn details(base: &str, namespace: &str, name: &str) -> Result<GalleryExtension, String> {
    let url = format!("{base}/api/{}/{}", encode(namespace), encode(name));
    let v = get_json(&format!("{url}/{}", target_platform())).or_else(|_| get_json(&url))?;
    GalleryExtension::parse(&v).ok_or_else(|| format!("{namespace}.{name} isn't in the marketplace"))
}

/// The SHA-256 of a file, in hex (`shasum`, which ships with macOS).
pub fn sha256_of(path: &Path) -> Result<String, String> {
    let out = Command::new("/usr/bin/shasum").args(["-a", "256"]).arg(path).output().map_err(|e| format!("Couldn't run shasum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().map(str::to_lowercase).filter(|h| h.len() == 64).ok_or_else(|| "Couldn't compute the package's checksum".into())
}

/// Downloads an extension's package to `dest`, checking it against the registry's SHA-256.
pub fn download(ext: &GalleryExtension, dest: &Path) -> Result<(), String> {
    let url = ext.download.as_deref().ok_or_else(|| format!("{} can't be downloaded", ext.display_name))?;
    let out = Command::new("/usr/bin/curl")
        .args(["-sSfL", "--max-time", "600", "-A", concat!("Orbvane/", env!("CARGO_PKG_VERSION")), "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| format!("Couldn't run curl: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(format!("Couldn't download {}: {}", ext.display_name, String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ")));
    }
    if let Some(sha) = &ext.sha256 {
        let expected = if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) { sha.to_lowercase() } else { get_text(sha)?.split_whitespace().next().unwrap_or("").to_lowercase() };
        let actual = sha256_of(dest)?;
        if !expected.is_empty() && expected != actual {
            let _ = std::fs::remove_file(dest);
            return Err(format!("The package of {} is damaged (its checksum doesn't match).", ext.display_name));
        }
    }
    Ok(())
}

/// A download in progress: where the package goes in the extensions folder.
pub fn download_path(registry_dir: &Path, ext: &GalleryExtension) -> std::path::PathBuf {
    registry_dir.join(format!(".download-{}-{}.vsix", ext.id(), ext.version))
}

impl Registry {
    /// Installs a package downloaded from Open VSX (and remembers where it came from, for
    /// updates). Returns its id.
    pub fn install_from_gallery(&mut self, vsix: &Path) -> Result<String, String> {
        self.install_from(vsix, Source::OpenVsx)
    }
}

/// For tests: a tiny local HTTP server standing in for the registry.
pub mod testing {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A local HTTP server answering GETs from the files `files(base)` returns (path → body);
    /// others get a 404. Returns its address.
    pub fn serve(files: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files = files(&base);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut stream = stream;
                match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        base
    }
}

#[cfg(test)]
mod tests {
    use super::testing::serve;
    use super::*;

    #[test]
    fn parses_queries() {
        let q = Query::parse(r#"dracula @category:"programming languages" @sort:rating"#);
        assert_eq!(q, Query { text: "dracula".into(), category: Some("programming languages".into()), sort: Sort::Rating });
        assert_eq!(Query::parse("@popular").sort, Sort::Installs);
        let url = search_url("https://x", &Query::parse("a b @category:Themes"), 0, 20);
        assert_eq!(url, "https://x/api/-/search?size=20&offset=0&includeAllVersions=false&query=a%20b&category=Themes&sortBy=relevance&sortOrder=desc");
        assert!(search_url("https://x", &Query::default(), 0, 20).ends_with("sortBy=downloadCount&sortOrder=desc"));
    }

    #[test]
    fn searches_downloads_and_installs() {
        // A package, as the registry would serve it.
        let root = std::env::temp_dir().join(format!("orbvane-gallery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("extension")).unwrap();
        std::fs::write(pkg.join("extension/package.json"), r#"{ "name": "theme-x", "publisher": "acme", "version": "1.1.0", "displayName": "Theme X" }"#).unwrap();
        let vsix = root.join("x.vsix");
        assert!(Command::new("/usr/bin/zip").current_dir(&pkg).args(["-q", "-r"]).arg(&vsix).arg("extension").status().unwrap().success());
        let bytes = std::fs::read(&vsix).unwrap();
        let sha = sha256_of(&vsix).unwrap();

        let entry = |base: &str, version: &str| {
            serde_json::json!({
                "namespace": "acme", "name": "theme-x", "version": version, "displayName": "Theme X",
                "description": "A theme.", "downloadCount": 1234, "averageRating": 4.5, "verified": true,
                "namespaceDisplayName": "Acme", "categories": ["Themes"], "targetPlatform": "universal",
                "files": { "download": format!("{base}/x.vsix"), "sha256": format!("{base}/x.sha256"), "icon": format!("{base}/icon.png") }
            })
        };
        let base = serve(|base| {
            vec![
                (search_url("", &Query::parse("theme"), 0, 10), serde_json::json!({ "offset": 0, "totalSize": 1, "extensions": [entry(base, "1.1.0")] }).to_string().into_bytes()),
                ("/api/acme/theme-x".to_string(), entry(base, "1.1.0").to_string().into_bytes()),
                ("/x.vsix".to_string(), bytes.clone()),
                ("/x.sha256".to_string(), format!("{sha}  x.vsix\n").into_bytes()),
            ]
        });

        let page = search(&base, &Query::parse("theme"), 0, 10).unwrap();
        assert_eq!(page.total, 1);
        let x = &page.extensions[0];
        assert_eq!((x.id().as_str(), x.publisher.as_str(), x.downloads, x.rating, x.verified), ("acme.theme-x", "Acme", 1234, Some(4.5), true));
        // No darwin build: the universal one.
        let d = details(&base, "acme", "theme-x").unwrap();
        assert_eq!(d.version, "1.1.0");
        assert!(details(&base, "acme", "nope").is_err());

        let mut reg = Registry::scan(&root.join("extensions"));
        std::fs::create_dir_all(&reg.dir).unwrap();
        let dest = download_path(&reg.dir, &d);
        download(&d, &dest).unwrap();
        let id = reg.install_from_gallery(&dest).unwrap();
        assert_eq!(id, "acme.theme-x");
        assert!(Registry::scan(&reg.dir).gallery.contains("acme.theme-x"));

        // A damaged download is refused.
        let mut bad = d.clone();
        bad.sha256 = Some(format!("{base}/x.vsix"));
        assert!(download(&bad, &dest).unwrap_err().contains("damaged"));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Against open-vsx.org (`cargo test -p extensions -- --ignored`).
    #[test]
    #[ignore]
    fn live_open_vsx() {
        let page = search(OPEN_VSX, &Query::parse("rust"), 0, 5).unwrap();
        assert!(page.total > 0 && !page.extensions.is_empty(), "{page:?}");
        let d = details(OPEN_VSX, "rust-lang", "rust-analyzer").unwrap();
        assert_eq!(d.target_platform, target_platform());
        assert!(d.download.is_some() && d.sha256.is_some());
        let popular = search(OPEN_VSX, &Query::default(), 0, 3).unwrap();
        assert!(popular.extensions[0].downloads >= popular.extensions[1].downloads);
    }
}
