//! The Assistant's chats on disk, per folder or workspace:
//! `<user data>/State/chats/<workspace key>/`, one `<id>.json` per chat (its transcript and the
//! agent's session id, to continue it) and `index.json` listing them for the History.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::assistant::Entry;
use super::session::{read, state_dir, workspace_key, write};

/// What the History lists about a chat.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub(super) struct Meta {
    pub id: String,
    pub title: String,
    /// The agent (`Choice::key`).
    pub agent: String,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub(super) struct Saved {
    #[serde(flatten)]
    pub meta: Meta,
    /// The agent's own session id, for `session/load`.
    pub session: Option<String>,
    pub entries: Vec<Entry>,
}

/// The folder for `workspace`'s chats.
pub(super) fn dir_for(workspace: Option<&Path>) -> PathBuf {
    state_dir().join("chats").join(workspace_key(workspace))
}

fn chat_file(dir: &Path, id: &str) -> PathBuf {
    // Ids are ours (UUIDs), but never let one name a path elsewhere.
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    dir.join(format!("{safe}.json"))
}

/// The chats saved in `dir`, newest first.
pub(super) fn list(dir: &Path) -> Vec<Meta> {
    let mut metas: Vec<Meta> = read(&dir.join("index.json"));
    metas.sort_by(|a, b| b.updated.cmp(&a.updated));
    metas
}

pub(super) fn load(dir: &Path, id: &str) -> Option<Saved> {
    let text = std::fs::read_to_string(chat_file(dir, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the chat and its line in the index.
pub(super) fn save(dir: &Path, chat: &Saved) {
    write(&chat_file(dir, &chat.meta.id), chat);
    let mut metas: Vec<Meta> = read(&dir.join("index.json"));
    metas.retain(|m| m.id != chat.meta.id);
    metas.push(chat.meta.clone());
    write(&dir.join("index.json"), &metas);
}

pub(super) fn delete(dir: &Path, id: &str) {
    let _ = std::fs::remove_file(chat_file(dir, id));
    let mut metas: Vec<Meta> = read(&dir.join("index.json"));
    metas.retain(|m| m.id != id);
    write(&dir.join("index.json"), &metas);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_lists_and_deletes() {
        let dir = std::env::temp_dir().join(format!("orbvane-chats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let chat = |id: &str, updated| Saved {
            meta: Meta { id: id.into(), title: format!("chat {id}"), agent: "codex".into(), created: 1, updated },
            session: Some("s".into()),
            entries: vec![Entry::User("hi".into()), Entry::Agent("hello".into())],
        };
        save(&dir, &chat("a", 5));
        save(&dir, &chat("b", 9));
        save(&dir, &chat("a", 12));
        assert_eq!(list(&dir).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        let a = load(&dir, "a").unwrap();
        assert_eq!((a.meta.updated, a.session.as_deref(), a.entries.len()), (12, Some("s"), 2));
        delete(&dir, "a");
        assert!(load(&dir, "a").is_none());
        assert_eq!(list(&dir).len(), 1);
        assert!(chat_file(&dir, "../x").ends_with("x.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
