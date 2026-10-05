//! Quick input: the command palette (`>` prefix), Go to File, pick lists like the color
//! theme picker (fuzzy matched), and input boxes that ask for a value (a branch name).

use std::path::{Path, PathBuf};

use crate::commands::Command;
use crate::workbench::{GitInput, GitPick};

#[derive(Clone, Debug)]
pub enum Action {
    Run(Command),
    Open(PathBuf),
    Theme(String),
    Git(GitPick),
    /// Go to a position in a file (references, symbols).
    Goto(PathBuf, text::Pos),
    /// Go to a position in the active editor (Go to Line, symbols of an untitled file).
    GotoHere(text::Pos),
    /// Open this folder (Open Recent).
    OpenFolder(PathBuf),
    /// Remove this folder from the workspace.
    RemoveRootFolder(PathBuf),
    /// Insert this snippet body at the selection (Insert Snippet).
    InsertSnippet(String),
    /// Record a new keybinding for the command (Keyboard Shortcuts).
    DefineKeybinding(Command),
    /// Start a debug configuration, or create launch.json for a debugger.
    Debug(crate::workbench::DebugPick),
    /// Run the task with this label.
    Task(String),
    /// Compare the active file with this one.
    CompareWith(PathBuf),
    /// Terminate the running task with this label.
    TerminateTask(String),
    /// Change the active editor's language.
    Language(language::Lang),
    /// An item of a quick pick an extension asked for (its index).
    ExtPick(usize),
    /// Choose the Assistant's agent (Assistant: Select Agent).
    Agent(crate::workbench::AgentAction),
}

/// What the quick input lists, from the prefix typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Files,
    /// `>`
    Commands,
    /// `@`: symbols in the active editor.
    Symbols,
    /// `#`: symbols in the workspace.
    WorkspaceSymbols,
    /// `:`: go to a line (and column).
    Line,
}

/// Asks for a text value instead of picking from a list.
pub struct InputBox {
    /// The message under the input ("Please provide a new branch name").
    pub prompt: String,
    pub placeholder: String,
    pub purpose: GitInput,
    /// Why the current value can't be accepted (drawn in the validation colors).
    pub error: Option<String>,
    /// Show the value as dots (passwords).
    pub password: bool,
}

pub struct Item {
    pub label: String,
    pub detail: String,
    /// Char indices in `label` that matched the query (drawn highlighted).
    pub matches: Vec<usize>,
    /// Keycaps for the command's shortcut (e.g. ["⇧", "⌘", "P"]).
    pub shortcut: Option<Vec<String>>,
    pub action: Action,
    /// Group label ("dark themes"); a separator is drawn above the first item of each group.
    pub group: Option<String>,
    /// An LSP `SymbolKind`, drawn as the symbol's icon.
    pub kind: Option<u32>,
}

/// A fixed list to choose from, filtered by the input.
pub struct Picker {
    pub placeholder: String,
    pub choices: Vec<Item>,
}

pub struct Palette {
    pub input: String,
    pub selected: usize,
    pub scroll: usize,
    pub items: Vec<Item>,
    pub picker: Option<Picker>,
    pub input_box: Option<InputBox>,
    /// Whether a git repository is open (git commands are listed only then).
    pub has_repo: bool,
    /// Shown instead of "No matching results" when the list is empty.
    pub message: Option<String>,
}

pub const MAX_VISIBLE: usize = 12;

impl Palette {
    pub fn new(input: &str) -> Self {
        Self { input: input.to_string(), selected: 0, scroll: 0, items: Vec::new(), picker: None, input_box: None, has_repo: false, message: None }
    }

    /// An input box, prefilled with `value`.
    pub fn with_input(input_box: InputBox, value: &str) -> Self {
        let mut p = Self::new(value);
        p.input_box = Some(input_box);
        p
    }

    pub fn with_picker(picker: Picker) -> Self {
        let mut p = Self::new("");
        p.picker = Some(picker);
        p.update(&[]);
        p
    }

    /// What the input lists (None for pickers and input boxes).
    pub fn mode(&self) -> Option<Mode> {
        if self.picker.is_some() || self.input_box.is_some() {
            return None;
        }
        Some(match self.input.chars().next() {
            Some('>') => Mode::Commands,
            Some('@') => Mode::Symbols,
            Some('#') => Mode::WorkspaceSymbols,
            Some(':') => Mode::Line,
            _ => Mode::Files,
        })
    }

    pub fn is_commands(&self) -> bool {
        self.mode() == Some(Mode::Commands)
    }

    pub fn is_files(&self) -> bool {
        self.mode() == Some(Mode::Files)
    }

    pub fn placeholder(&self) -> &str {
        if let Some(b) = &self.input_box {
            return &b.placeholder;
        }
        match &self.picker {
            Some(p) => &p.placeholder,
            None if !self.is_files() => "",
            None => "Search files by name (append : to go to line, > for commands)",
        }
    }

    /// Whether the item at `idx` starts a new group (and gets a separator label).
    pub fn starts_group(&self, idx: usize) -> bool {
        let group = |i: usize| self.items.get(i).and_then(|it| it.group.as_deref());
        group(idx).is_some() && (idx == 0 || group(idx - 1) != group(idx))
    }

    /// `files`: each file with its label relative to the workspace ("folder/rel" with several folders).
    pub fn update(&mut self, files: &[(PathBuf, String)]) {
        self.items.clear();
        self.selected = 0;
        self.scroll = 0;
        self.message = None;
        if self.input_box.is_some() || matches!(self.mode(), Some(Mode::Symbols | Mode::WorkspaceSymbols | Mode::Line)) {
            return; // filled in by the workbench
        }
        if let Some(picker) = &self.picker {
            let query = self.input.trim();
            self.items = picker
                .choices
                .iter()
                .filter_map(|c| {
                    let (_, matches) = fuzzy(query, &c.label)?;
                    Some(Item {
                        label: c.label.clone(),
                        detail: c.detail.clone(),
                        matches,
                        shortcut: c.shortcut.clone(),
                        action: c.action.clone(),
                        group: c.group.clone(),
                        kind: c.kind,
                    })
                })
                .collect();
        } else if let Some(query) = self.input.strip_prefix('>') {
            let query = query.trim();
            let mut scored: Vec<(i32, Item)> = Command::all()
                .into_iter()
                .filter(|c| *c != Command::CommandPalette && (self.has_repo || !c.needs_repo()))
                .filter_map(|cmd| {
                    let (score, matches) = fuzzy(query, cmd.title())?;
                    Some((score, Item {
                        label: cmd.title().to_string(),
                        detail: String::new(),
                        matches,
                        shortcut: crate::keymap::keycaps(cmd),
                        action: Action::Run(cmd),
                        group: None,
                        kind: None,
                    }))
                })
                .collect();
            if !query.is_empty() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            self.items = scored.into_iter().map(|(_, i)| i).collect();
        } else {
            let query = self.input.trim();
            let mut scored: Vec<(i32, Item)> = files
                .iter()
                .filter_map(|(path, rel)| {
                    let name = path.file_name()?.to_string_lossy().to_string();
                    let rel = Path::new(rel);
                    let dir = rel.parent().map(|p| p.display().to_string()).unwrap_or_default();
                    // Prefer matches in the file name; fall back to the whole relative path.
                    let (score, matches) = match fuzzy(query, &name) {
                        Some((s, m)) => (s + 100, m),
                        None => {
                            let (s, _) = fuzzy(query, &rel.to_string_lossy())?;
                            (s, Vec::new())
                        }
                    };
                    Some((score, Item { label: name, detail: dir, matches, shortcut: None, action: Action::Open(path.clone()), group: None, kind: None }))
                })
                .collect();
            if query.is_empty() {
                scored.truncate(200);
            } else {
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.len().cmp(&b.1.label.len())));
            }
            self.items = scored.into_iter().map(|(_, i)| i).take(500).collect();
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        self.scroll_to_selection();
    }

    pub fn scroll_to_selection(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE {
            self.scroll = self.selected + 1 - MAX_VISIBLE;
        }
    }

    pub fn selected_action(&self) -> Option<Action> {
        self.items.get(self.selected).map(|i| i.action.clone())
    }
}

/// Subsequence fuzzy match. Returns a score (higher is better) and matched char indices.
pub fn fuzzy(query: &str, target: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let t: Vec<char> = target.chars().collect();
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    let mut matches = Vec::with_capacity(q.len());
    let mut score = 0i32;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    let eq = |i: usize, qc: char| t[i].to_lowercase().eq(std::iter::once(qc));
    for (qi, &qc) in q.iter().enumerate() {
        let mut found = None;
        // Prefer a word-start match ahead (unless we're continuing a run), but only if the
        // rest of the query can still match after it.
        let next = (ti..t.len()).find(|&i| eq(i, qc))?;
        if prev.is_none_or(|p| next != p + 1) {
            let rest = &q[qi + 1..];
            found = (next..t.len()).find(|&i| eq(i, qc) && is_word_start(&t, i) && is_subsequence(rest, &t[i + 1..]));
        }
        let i = found.unwrap_or(next);
        score += 1;
        if prev.is_some_and(|p| i == p + 1) {
            score += 5;
        }
        if is_word_start(&t, i) {
            score += 8;
        }
        if let Some(p) = prev {
            score -= ((i - p - 1) as i32).min(5);
        }
        matches.push(i);
        prev = Some(i);
        ti = i + 1;
    }
    score -= (t.len() as i32 - q.len() as i32).max(0) / 8;
    Some((score, matches))
}

fn is_subsequence(q: &[char], t: &[char]) -> bool {
    let mut it = t.iter().flat_map(|c| c.to_lowercase());
    q.iter().all(|qc| it.any(|c| c == *qc))
}

fn is_word_start(t: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let (p, c) = (t[i - 1], t[i]);
    matches!(p, ' ' | '_' | '-' | '.' | '/' | ':') || (p.is_lowercase() && c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_basics() {
        assert!(fuzzy("tpsb", "View: Toggle Primary Side Bar").is_some());
        let (_, m) = fuzzy("split", "View: Split Editor").unwrap();
        assert_eq!(m, vec![6, 7, 8, 9, 10]);
        assert!(fuzzy("xyz", "main.rs").is_none());
        let (a, _) = fuzzy("main", "main.rs").unwrap();
        let (b, _) = fuzzy("main", "domain_model.rs").unwrap();
        assert!(a > b);
    }
}
