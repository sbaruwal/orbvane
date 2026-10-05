//! Multi-root workspaces, stored as `.code-workspace` files: a JSONC object with
//! `folders` (each a `path`, relative to the file or absolute, and an optional `name`) and
//! workspace `settings`. A workspace that hasn't been saved yet lives in the user data
//! folder ("Untitled (Workspace)"), like the standard untitled workspaces.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

pub const EXTENSION: &str = "code-workspace";

/// A workspace folder: its path and the name to show (None: the folder's own name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub name: Option<String>,
}

pub fn is_workspace_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == EXTENSION)
}

/// Where untitled workspaces are kept.
fn untitled_dir() -> PathBuf {
    settings::user_data_dir().join("Workspaces")
}

/// A new untitled workspace file's path (not created yet).
pub fn new_untitled() -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    untitled_dir().join(stamp.to_string()).join(format!("workspace.{EXTENSION}"))
}

pub fn is_untitled(file: &Path) -> bool {
    file.starts_with(untitled_dir())
}

/// The workspace's name: the file's name without the extension, or "Untitled".
pub fn name(file: &Path) -> String {
    if is_untitled(file) {
        return "Untitled".into();
    }
    file.file_stem().map_or_else(|| "Untitled".into(), |s| s.to_string_lossy().into_owned())
}

/// Resolves `.` and `..` without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// `path` relative to `base` (both absolute), with `..` where needed.
pub fn relative(path: &Path, base: &Path) -> PathBuf {
    let (p, b): (Vec<_>, Vec<_>) = (path.components().collect(), base.components().collect());
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common <= 1 {
        return path.to_path_buf(); // nothing but the root in common
    }
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// The folders listed in a workspace file's text.
pub fn parse(text: &str, file: &Path) -> Result<Vec<Folder>, String> {
    let value: Value = serde_json::from_str(&theme::strip_jsonc(text)).map_err(|e| format!("Unable to parse the workspace file: {e}"))?;
    let dir = file.parent().unwrap_or(Path::new("/"));
    let folders = value["folders"].as_array().cloned().unwrap_or_default();
    Ok(folders
        .iter()
        .filter_map(|f| {
            let path = match (f["path"].as_str(), f["uri"].as_str()) {
                (Some(p), _) => PathBuf::from(p),
                (None, Some(uri)) => PathBuf::from(uri.strip_prefix("file://")?),
                _ => return None,
            };
            let path = normalize(&if path.is_absolute() { path } else { dir.join(path) });
            Some(Folder { path, name: f["name"].as_str().map(String::from) })
        })
        .collect())
}

/// Reads a workspace file's folders.
pub fn read(file: &Path) -> Result<Vec<Folder>, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("Unable to read {}: {e}", file.display()))?;
    parse(&text, file)
}

/// `text` (a workspace file, possibly empty) with its folders replaced, keeping everything
/// else (settings, comments). Paths are written relative to the file.
pub fn with_folders(text: &str, folders: &[Folder], file: &Path) -> String {
    let dir = file.parent().unwrap_or(Path::new("/"));
    let list: Vec<Value> = folders
        .iter()
        .map(|f| {
            let path = if is_untitled(file) { f.path.clone() } else { relative(&f.path, dir) };
            let mut v = json!({ "path": path.to_string_lossy() });
            if let Some(n) = &f.name {
                v["name"] = json!(n);
            }
            v
        })
        .collect();
    if text.trim().is_empty() {
        let v = json!({ "folders": list, "settings": {} });
        return serde_json::to_string_pretty(&v).unwrap_or_default().replace("  ", "\t") + "\n";
    }
    settings::jsonc::set_property(text, "folders", Some(&Value::Array(list)))
}

/// Writes (or rewrites) a workspace file's folders.
pub fn write(file: &Path, folders: &[Folder]) -> Result<(), String> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(file, with_folders(&text, folders, file)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_relative_and_absolute_folders() {
        let text = r#"{
            // mine
            "folders": [ { "path": "app" }, { "path": "../lib", "name": "Library" }, { "path": "/abs/x" }, { "uri": "file:///u/y" } ],
            "settings": { "editor.tabSize": 2 }
        }"#;
        let f = parse(text, Path::new("/work/ws/main.code-workspace")).unwrap();
        let paths: Vec<_> = f.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, ["/work/ws/app", "/work/lib", "/abs/x", "/u/y"].map(PathBuf::from));
        assert_eq!(f[1].name.as_deref(), Some("Library"));
    }

    #[test]
    fn writes_folders_relative_to_the_file() {
        let file = Path::new("/work/ws/main.code-workspace");
        let folders = [Folder { path: "/work/ws/app".into(), name: None }, Folder { path: "/work/lib".into(), name: Some("Lib".into()) }];
        let text = with_folders("", &folders, file);
        assert_eq!(parse(&text, file).unwrap(), folders);
        assert!(text.contains("\"path\": \"../lib\"") && text.contains("\"settings\""), "{text}");
        // Other keys and comments are kept.
        let text = with_folders("{\n\t// note\n\t\"settings\": { \"a\": 1 },\n\t\"folders\": []\n}", &folders[..1], file);
        assert!(text.contains("// note") && text.contains("\"a\": 1") && text.contains("\"path\": \"app\""), "{text}");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative(Path::new("/a/b/c"), Path::new("/a/b")), PathBuf::from("c"));
        assert_eq!(relative(Path::new("/a/x"), Path::new("/a/b")), PathBuf::from("../x"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), PathBuf::from("."));
        assert_eq!(relative(Path::new("/x/y"), Path::new("/a/b")), PathBuf::from("/x/y"));
    }
}
