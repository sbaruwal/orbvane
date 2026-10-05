//! Change Language Mode (⌘K M, or the language in the status bar): picks the active editor's
//! language by hand. The document gets that language's highlighting and language server.

use language::{Highlighter, Lang};

use super::Workbench;
use crate::palette::{Action, Item, Palette, Picker};

impl Workbench {
    pub(super) fn change_language_mode(&mut self) {
        let Some(doc) = self.active_editor().filter(|e| !e.is_special()).and_then(|e| self.docs[e.doc].as_ref()) else { return };
        let current = doc.lang;
        let mut langs: Vec<Lang> = Lang::all().filter(|l| *l != Lang::SearchResult).collect();
        langs.sort_by_key(|l| l.name().to_lowercase());
        // The current language first.
        if let Some(i) = langs.iter().position(|l| *l == current) {
            let l = langs.remove(i);
            langs.insert(0, l);
        }
        let choices = langs
            .into_iter()
            .map(|l| Item {
                label: l.name().to_string(),
                detail: if l == current { format!("({}) - Configured Language", l.id()) } else { format!("({})", l.id()) },
                matches: Vec::new(),
                shortcut: None,
                action: Action::Language(l),
                group: None,
                kind: None,
            })
            .collect();
        self.palette = Some(Palette::with_picker(Picker { placeholder: "Select Language Mode".into(), choices }));
    }

    /// Switches the active editor's document to `lang`.
    pub(super) fn set_language(&mut self, lang: Lang) {
        let Some(id) = self.active_editor().map(|e| e.doc) else { return };
        let Some(doc) = self.docs[id].as_mut().filter(|d| d.lang != lang) else { return };
        doc.lang = lang;
        doc.highlight = Highlighter::with_parser(lang, !doc.large);
        if let Some(path) = doc.buffer.path().map(std::path::Path::to_path_buf) {
            // Reopened with the new language's server on the next frame.
            self.lsp.diagnostics.remove(&path);
            self.close_lsp_doc(&path);
        }
    }
}
