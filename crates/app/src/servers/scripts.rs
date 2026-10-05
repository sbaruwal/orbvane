//! JavaScript inside HTML files. Each HTML file with scripts gets a companion document for the
//! JavaScript server (`<file>.script.js` next to it, never written to disk): the file's text with
//! everything outside its scripts blanked, so a position means the same in both. Requests made
//! inside a script go to that document, and its diagnostics are merged into the file's.

use std::path::{Path, PathBuf};

use language::Lang;
use lsp::Encoding;
use serde_json::json;
use text::{Buffer, Pos};

use super::{OpenDoc, ServerKey, Servers};

/// An HTML file's companion document.
pub(super) struct Embedded {
    pub(super) virt: PathBuf,
    /// The file's buffer version, text and scripts (byte ranges) when it was last sent.
    version: u64,
    text: String,
    scripts: Vec<(usize, usize)>,
    /// The file's own server's diagnostics, and the scripts' (in the file's server's encoding).
    own: Option<(Encoding, Vec<lsp::Diagnostic>)>,
    script: Vec<lsp::Diagnostic>,
}

/// The companion document's path.
fn script_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".script.js");
    path.with_file_name(name)
}

/// `text` with everything outside `ranges` turned into spaces (line breaks kept), each char
/// as wide as it is in `encoding`, so columns stay the same.
pub(super) fn blank_outside(text: &str, ranges: &[(usize, usize)], encoding: Encoding) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let blank = |s: &str, out: &mut String| {
        for c in s.chars() {
            match c {
                '\n' | '\r' => out.push(c),
                _ => {
                    let n = if encoding == Encoding::Utf16 { c.len_utf16() } else { c.len_utf8() };
                    out.extend(std::iter::repeat_n(' ', n));
                }
            }
        }
    };
    for &(a, b) in ranges {
        blank(&text[at..a], &mut out);
        out.push_str(&text[a..b]);
        at = b;
    }
    blank(&text[at..], &mut out);
    out
}

/// Moves `d`'s columns from `from` units to `to` units, reading the lines of `text`.
fn convert(d: &mut lsp::Diagnostic, text: &str, from: Encoding, to: Encoding) {
    if from == to {
        return;
    }
    for p in [&mut d.range.start, &mut d.range.end] {
        let line = text.split('\n').nth(p.line as usize).unwrap_or("");
        p.character = to.to_lsp(line, from.from_lsp(line, p.character));
    }
}

impl Servers {
    /// Opens or updates `path`'s companion document (an HTML file at `root`) if it has
    /// scripts or had them.
    pub(super) fn sync_scripts(&mut self, path: &Path, buffer: &Buffer, root: &Path) {
        let version = buffer.version();
        if self.embedded.get(path).is_some_and(|e| e.version == version && self.docs.contains_key(&e.virt)) {
            return;
        }
        let text = buffer.text();
        let scripts = html::features::scripts(&html::parse::Document::parse(&text));
        if scripts.is_empty() && !self.embedded.contains_key(path) {
            return;
        }
        let Some(key) = Lang::from_name("javascript").and_then(|js| self.ensure_client(js, root)) else { return };
        let encoding = self.clients.get(&key).map_or(Encoding::Utf16, |c| c.encoding);
        let shadow = blank_outside(&text, &scripts, encoding);
        let virt = script_path(path);
        // The file's server may have published before the file had scripts.
        let own = self.diagnostics.get(path).cloned();
        let e = self.embedded.entry(path.to_path_buf()).or_insert_with(|| Embedded {
            virt: virt.clone(),
            version,
            text: String::new(),
            scripts: Vec::new(),
            own,
            script: Vec::new(),
        });
        (e.version, e.text, e.scripts) = (version, text, scripts);
        self.host_of.insert(virt.clone(), path.to_path_buf());
        let uri = lsp::path_to_uri(&virt);
        match self.docs.get_mut(&virt) {
            Some(doc) => {
                doc.version += 1;
                doc.buffer_version = version;
                let params = json!({ "textDocument": { "uri": uri, "version": doc.version }, "contentChanges": [{ "text": shadow }] });
                if let Some(client) = self.clients.get_mut(&doc.key) {
                    client.notify("textDocument/didChange", params);
                }
            }
            None => {
                if let Some(client) = self.clients.get_mut(&key) {
                    let params = json!({ "textDocument": { "uri": uri, "languageId": "javascript", "version": 1, "text": shadow } });
                    client.notify("textDocument/didOpen", params);
                }
                self.docs.insert(virt, OpenDoc { key, version: 1, buffer_version: version });
            }
        }
    }

    /// Where a request at `pos` of `path` goes: the companion document when `pos` is in a
    /// script, else `path` itself.
    pub(super) fn target(&mut self, path: &Path, buffer: &Buffer, pos: Pos) -> PathBuf {
        if self.embedded.contains_key(path) {
            if let Some(root) = self.docs.get(path).map(|d| d.key.1.clone()) {
                self.sync_scripts(path, buffer, &root);
            }
            let e = &self.embedded[path];
            let at = buffer.byte_of(buffer.clamp(pos));
            if e.scripts.iter().any(|&(a, b)| a <= at && at <= b) && self.docs.contains_key(&e.virt) {
                return e.virt.clone();
            }
        }
        path.to_path_buf()
    }

    /// The companion document of `path`, if it's open.
    pub(super) fn script_doc(&self, path: &Path) -> Option<&Path> {
        self.embedded.get(path).map(|e| e.virt.as_path()).filter(|v| self.docs.contains_key(*v))
    }

    /// The HTML file whose companion document is `path`.
    pub(super) fn host_of(&self, path: &Path) -> Option<&Path> {
        self.host_of.get(path).map(PathBuf::as_path)
    }

    /// Closes `path`'s companion document.
    pub(super) fn close_scripts(&mut self, path: &Path) {
        if let Some(e) = self.embedded.remove(path) {
            if let Some(doc) = self.docs.remove(&e.virt) {
                if let Some(client) = self.clients.get_mut(&doc.key) {
                    client.notify("textDocument/didClose", json!({ "textDocument": { "uri": lsp::path_to_uri(&e.virt) } }));
                }
            }
        }
    }

    /// Diagnostics published by server `key` for `path`, an HTML file with scripts or a
    /// companion document.
    pub(super) fn publish_merged(&mut self, key: &ServerKey, encoding: Encoding, path: &Path, mut diags: Vec<lsp::Diagnostic>) {
        if let Some(host) = self.host_of.get(path).cloned() {
            let to = self.docs.get(&host).and_then(|d| self.clients.get(&d.key)).map_or(encoding, |c| c.encoding);
            // A companion document closed since: its diagnostics are dropped.
            let Some(e) = self.embedded.get_mut(&host) else { return };
            for d in &mut diags {
                convert(d, &e.text, encoding, to);
            }
            e.script = diags;
            return self.merge_diagnostics(&host);
        }
        if let Some(e) = self.embedded.get_mut(path) {
            e.own = Some((encoding, diags));
            self.diagnostics_from.insert(path.to_path_buf(), key.clone());
            self.merge_diagnostics(path);
        }
    }

    /// Server `key` stopped: the scripts' diagnostics it published are gone.
    pub(super) fn drop_script_diagnostics(&mut self, key: &ServerKey) {
        let hosts: Vec<PathBuf> = self.embedded.iter().filter(|(_, e)| self.docs.get(&e.virt).is_some_and(|d| d.key == *key)).map(|(p, _)| p.clone()).collect();
        for host in hosts {
            if let Some(e) = self.embedded.get_mut(&host) {
                e.script.clear();
            }
            self.merge_diagnostics(&host);
        }
    }

    fn merge_diagnostics(&mut self, path: &Path) {
        let host = self.docs.get(path).and_then(|d| self.clients.get(&d.key)).map(|c| c.encoding);
        let Some(e) = self.embedded.get(path) else { return };
        let encoding = e.own.as_ref().map(|o| o.0).or(host).unwrap_or(Encoding::Utf8);
        let all: Vec<lsp::Diagnostic> = e.own.iter().flat_map(|o| o.1.iter()).chain(&e.script).cloned().collect();
        self.published.push(path.to_path_buf());
        if all.is_empty() {
            self.diagnostics.remove(path);
        } else {
            self.diagnostics.insert(path.to_path_buf(), (encoding, all));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_all_but_the_scripts() {
        let text = "<p>é😀</p>\n<script>let a = 1;</script>";
        let a = text.find("let").unwrap();
        let b = text.find("</script>").unwrap();
        let utf16 = blank_outside(text, &[(a, b)], Encoding::Utf16);
        assert_eq!(utf16, format!("{}\n{}let a = 1;{}", " ".repeat(10), " ".repeat(8), " ".repeat(9)));
        // Columns in the server's units are the same in both.
        assert_eq!(utf16.lines().next().unwrap().len(), Encoding::Utf16.to_lsp(text.lines().next().unwrap(), 9) as usize);
        let utf8 = blank_outside(text, &[(a, b)], Encoding::Utf8);
        assert_eq!(utf8.len(), text.len());
    }

    #[test]
    fn converts_script_diagnostics_to_the_files_encoding() {
        let text = "<b>😀</b><script>x(</script>";
        let chars = text[..text.find("x(").unwrap()].chars().count();
        let col16 = Encoding::Utf16.to_lsp(text, chars);
        let at = |c| lsp::Position { line: 0, character: c };
        let mut d = lsp::Diagnostic {
            range: lsp::Range { start: at(col16), end: at(col16 + 1) },
            severity: lsp::Severity::Error,
            message: String::new(),
            source: None,
            code: None,
            raw: serde_json::Value::Null,
        };
        convert(&mut d, text, Encoding::Utf16, Encoding::Utf8);
        assert_eq!(d.range.start.character as usize, text.find("x(").unwrap());
    }

    /// With a real typescript-language-server (skipped without one): a script's syntax error
    /// shows on the HTML file next to the file's own CSS warning, and completion in a script
    /// comes from the JavaScript server.
    #[test]
    fn scripts_with_a_real_javascript_server() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("orbvane-scripts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("index.html");
        std::fs::write(&path, "<p>é😀</p><style>p { colr: red }</style>\n<script>\nconst greeting = 'hi';\ngreeting.\n</script><script>let = ;</script>\n").unwrap();
        let buffer = Buffer::open(&path).unwrap();
        let (html, js) = (Lang::from_name("html").unwrap(), Lang::from_name("javascript").unwrap());
        let key = Servers::key_of(js, &dir).unwrap();
        let mut lsp = Servers::new(std::sync::Arc::new(|| {}));
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut items = Vec::new();
        let mut asked = false;
        while Instant::now() < deadline {
            lsp.sync(&path, html, &buffer, &dir);
            if lsp.ready(&key).is_none() || !lsp.take_missing().is_empty() {
                eprintln!("skipped: no typescript-language-server");
                return;
            }
            for e in lsp.poll() {
                if let crate::servers::Event::Completion { items: got, .. } = e {
                    items = got;
                }
            }
            let diags = lsp.diagnostics.get(&path).map(|d| d.1.clone()).unwrap_or_default();
            let css = diags.iter().any(|d| d.range.start.line == 0 && d.message.contains("colr"));
            let script = diags.iter().find(|d| d.range.start.line == 4 && d.severity == lsp::Severity::Error);
            if css && script.is_some() && !asked && lsp.ready(&key) == Some(true) {
                // `let` on the last line: the error starts after it (UTF-8 columns, as the file's).
                assert!(script.unwrap().range.start.character as usize >= "</script><script>let".len(), "{diags:?}");
                lsp.completion(&path, &buffer, Pos::new(3, 9), Some("."), 1);
                asked = true;
            }
            if items.iter().any(|i| i.label == "toUpperCase") {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("diagnostics: {:?}\ncompletions: {}", lsp.diagnostics.get(&path), items.len());
    }
}
