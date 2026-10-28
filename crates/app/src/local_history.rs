//! Local history: a copy of a file each time it's saved, kept outside the project in
//! `<user data>/History/<hash of the path>/` (one `<id>.txt` per copy and `entries.json` naming
//! the file and listing them, oldest first). The Timeline shows them with the file's commits.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One kept copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Its file's name in the folder.
    pub id: String,
    /// When it was kept, in milliseconds since the epoch.
    pub timestamp: i64,
    /// Why ("File Saved", "File Restored").
    pub source: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    resource: String,
    entries: Vec<Entry>,
}

/// Where local history is kept.
pub fn root() -> PathBuf {
    settings::user_data_dir().join("History")
}

/// The folder of `path`'s copies.
pub fn folder(root: &Path, path: &Path) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    root.join(format!("{h:016x}"))
}

fn read_index(dir: &Path) -> Index {
    std::fs::read_to_string(dir.join("entries.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// `path`'s copies, newest first.
pub fn entries(root: &Path, path: &Path) -> Vec<Entry> {
    let index = read_index(&folder(root, path));
    if index.resource != path.to_string_lossy() {
        return Vec::new();
    }
    let mut entries = index.entries;
    entries.reverse();
    entries
}

/// The file holding a copy.
pub fn file_of(root: &Path, path: &Path, entry: &Entry) -> PathBuf {
    // Ids are ours, but never let one name a path elsewhere.
    let safe: String = entry.id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    folder(root, path).join(safe)
}

/// Keeps `text` as `path`'s newest copy, unless it's the same as the newest one. At most `max`
/// copies are kept (the oldest go). Returns whether one was added.
pub fn record(root: &Path, path: &Path, text: &str, source: &str, max: usize) -> io::Result<bool> {
    let dir = folder(root, path);
    let mut index = read_index(&dir);
    if index.resource != path.to_string_lossy() {
        index = Index { resource: path.to_string_lossy().into_owned(), entries: Vec::new() };
    }
    if let Some(last) = index.entries.last() {
        if std::fs::read_to_string(file_of(root, path, last)).is_ok_and(|t| t == text) {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(&dir)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    // Two copies in the same millisecond get different names.
    let mut id = format!("{now}.txt");
    let mut n = 1;
    while index.entries.iter().any(|e| e.id == id) {
        id = format!("{now}-{n}.txt");
        n += 1;
    }
    std::fs::write(dir.join(&id), text)?;
    index.entries.push(Entry { id, timestamp: now, source: source.into() });
    let excess = index.entries.len().saturating_sub(max.max(1));
    for old in index.entries.drain(..excess) {
        let _ = std::fs::remove_file(file_of(root, path, &old));
    }
    std::fs::write(dir.join("entries.json"), serde_json::to_string_pretty(&index).map_err(io::Error::other)?)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_copies_newest_first() {
        let root = std::env::temp_dir().join(format!("orbvane-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = Path::new("/w/src/main.rs");
        assert!(entries(&root, path).is_empty());
        assert!(record(&root, path, "one", "File Saved", 3).unwrap());
        // The same text again: nothing new.
        assert!(!record(&root, path, "one", "File Saved", 3).unwrap());
        for text in ["two", "three", "four"] {
            assert!(record(&root, path, text, "File Saved", 3).unwrap());
        }
        let list = entries(&root, path);
        let texts: Vec<String> = list.iter().map(|e| std::fs::read_to_string(file_of(&root, path, e)).unwrap()).collect();
        assert_eq!(texts, ["four", "three", "two"]);
        assert_eq!(list[0].source, "File Saved");
        // The oldest copy's file went with it.
        assert_eq!(std::fs::read_dir(folder(&root, path)).unwrap().count(), 4);
        assert!(entries(&root, Path::new("/w/other.rs")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
