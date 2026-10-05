//! Word Count: an example Orbvane extension. It keeps a count of the active Markdown or text
//! file in the status bar and adds a few commands that use the editor's quick pick, input box,
//! notifications, output channels and edits.

use orbvane_extension::{Alignment, Context, Document, Extension, InputBoxOptions, MessageType, Position, QuickPickItem, StatusBarItem, TextEditor, WorkspaceEdit};
use serde_json::{json, Value};

const LANGUAGES: &[&str] = &["markdown", "plaintext"];
const OUTPUT: &str = "Word Count";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Words,
    Characters,
    Lines,
}

impl Mode {
    fn from_setting(v: &Value) -> Mode {
        match v.as_str() {
            Some("characters") => Mode::Characters,
            Some("lines") => Mode::Lines,
            _ => Mode::Words,
        }
    }

    fn unit(self, n: usize) -> String {
        let (one, many) = match self {
            Mode::Words => ("Word", "Words"),
            Mode::Characters => ("Character", "Characters"),
            Mode::Lines => ("Line", "Lines"),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }
}

/// The counts of a text.
#[derive(Debug, PartialEq)]
struct Counts {
    words: usize,
    characters: usize,
    lines: usize,
}

fn count(text: &str, ignored: &[String]) -> Counts {
    let words = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty() && !ignored.iter().any(|i| i.eq_ignore_ascii_case(w)))
        .count();
    Counts { words, characters: text.chars().count(), lines: if text.is_empty() { 0 } else { text.lines().count() } }
}

/// The selected text of an editor (all selections), if anything is selected.
fn selected_text(editor: &TextEditor, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let slice = |line: u32, from: u32, to: Option<u32>| {
        let l = lines.get(line as usize).copied().unwrap_or("");
        let from = (from as usize).min(l.len());
        let to = to.map_or(l.len(), |t| (t as usize).min(l.len())).max(from);
        l.get(from..to).unwrap_or("").to_string()
    };
    let parts: Vec<String> = editor
        .selections
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.start.line == s.end.line {
                return slice(s.start.line, s.start.character, Some(s.end.character));
            }
            let mut out = vec![slice(s.start.line, s.start.character, None)];
            out.extend((s.start.line + 1..s.end.line).map(|l| slice(l, 0, None)));
            out.push(slice(s.end.line, 0, Some(s.end.character)));
            out.join("\n")
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

struct WordCount {
    mode: Mode,
    show: bool,
    ignored: Vec<String>,
}

impl WordCount {
    fn read_settings(&mut self, ctx: &mut Context) {
        let s = ctx.configuration("wordCount");
        self.mode = Mode::from_setting(&s["mode"]);
        self.show = s["showInStatusBar"].as_bool().unwrap_or(true);
        self.ignored = s["ignoredWords"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    }

    fn counted(&self, doc: &Document) -> bool {
        LANGUAGES.contains(&doc.language_id.as_str())
    }

    /// Shows the active editor's count (of the selection, if any) in the status bar.
    fn update(&mut self, ctx: &mut Context) {
        let editor = ctx.active_text_editor().filter(|e| self.counted(&e.document));
        let Some(editor) = editor.filter(|_| self.show) else {
            ctx.remove_status_bar_item("count");
            return;
        };
        let text = editor.document.text.clone().unwrap_or_default();
        let selected = selected_text(&editor, &text);
        let counts = count(selected.as_deref().unwrap_or(&text), &self.ignored);
        let n = match self.mode {
            Mode::Words => counts.words,
            Mode::Characters => counts.characters,
            Mode::Lines => counts.lines,
        };
        let suffix = if selected.is_some() { " Selected" } else { "" };
        ctx.set_status_bar_item(&StatusBarItem {
            id: "count".into(),
            text: format!("$(edit) {}{suffix}", self.mode.unit(n)),
            tooltip: "Word Count".into(),
            command: Some("wordCount.show".into()),
            alignment: Alignment::Right,
            priority: 100,
        });
    }

    fn active_text(&self, ctx: &mut Context) -> Result<(TextEditor, String), String> {
        let editor = ctx.active_text_editor().ok_or("Open a file to count its words.")?;
        let text = editor.document.text.clone().unwrap_or_default();
        Ok((editor, text))
    }

    fn show(&mut self, ctx: &mut Context) -> Result<Value, String> {
        let (editor, text) = self.active_text(ctx)?;
        let c = count(&text, &self.ignored);
        ctx.show_information_message(&format!("{}: {} words, {} characters, {} lines.", editor.document.name, c.words, c.characters, c.lines));
        Ok(json!({ "words": c.words, "characters": c.characters, "lines": c.lines }))
    }

    fn choose_mode(&mut self, ctx: &mut Context) -> Result<Value, String> {
        let items = [
            QuickPickItem::new("Words").description("words").detail("Count the words, skipping wordCount.ignoredWords"),
            QuickPickItem::new("Characters").description("characters"),
            QuickPickItem::new("Lines").description("lines"),
        ];
        let Some(i) = ctx.show_quick_pick(&items, "What should the status bar count?") else { return Ok(Value::Null) };
        let value = ["words", "characters", "lines"][i];
        ctx.update_configuration("wordCount.mode", Some(json!(value)), false)?;
        self.mode = Mode::from_setting(&json!(value));
        self.update(ctx);
        Ok(json!(value))
    }

    fn find(&mut self, ctx: &mut Context) -> Result<Value, String> {
        let (editor, text) = self.active_text(ctx)?;
        let options = InputBoxOptions { prompt: "The word to count".into(), placeholder: "word".into(), ..Default::default() };
        let Some(word) = ctx.show_input_box(&options).filter(|w| !w.trim().is_empty()) else { return Ok(Value::Null) };
        let word = word.trim().to_lowercase();
        let hits: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| l.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w == word)).collect();
        let n: usize = text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| *w == word).count();
        let times = if n == 1 { "once".to_string() } else { format!("{n} times") };
        let msg = format!("'{word}' appears {times} in {}.", editor.document.name);
        if n > 0 && ctx.show_message_request(MessageType::Info, &msg, &["Show Lines"]).is_some() {
            ctx.clear_output(OUTPUT);
            ctx.append_output_line(OUTPUT, &format!("'{word}' in {}:", editor.document.name));
            for (line, l) in &hits {
                ctx.append_output_line(OUTPUT, &format!("{:>5}: {}", line + 1, l.trim()));
            }
            ctx.show_output(OUTPUT, true);
        } else if n == 0 {
            ctx.show_information_message(&msg);
        }
        Ok(json!(n))
    }

    fn insert_summary(&mut self, ctx: &mut Context) -> Result<Value, String> {
        let (editor, text) = self.active_text(ctx)?;
        let path = editor.document.path.clone().ok_or("Save the file first.")?;
        let c = count(&text, &self.ignored);
        let at: Position = editor.selection().start;
        let mut edit = WorkspaceEdit::new();
        edit.insert(path, at, format!("{} words, {} characters, {} lines", c.words, c.characters, c.lines));
        if !ctx.apply_edit(&edit) {
            return Err("The edit couldn't be applied.".into());
        }
        Ok(Value::Null)
    }
}

impl Extension for WordCount {
    fn activate(ctx: &mut Context) -> Self {
        let mut ext = WordCount { mode: Mode::Words, show: true, ignored: Vec::new() };
        ext.read_settings(ctx);
        ctx.log(&format!("Word Count is active ({}).", ctx.extension_path.display()));
        ext.update(ctx);
        ext
    }

    fn command(&mut self, ctx: &mut Context, command: &str, _args: &[Value]) -> Result<Value, String> {
        match command {
            "wordCount.show" => self.show(ctx),
            "wordCount.chooseMode" => self.choose_mode(ctx),
            "wordCount.find" => self.find(ctx),
            "wordCount.insertSummary" => self.insert_summary(ctx),
            _ => Err(format!("command '{command}' not found")),
        }
    }

    fn did_change(&mut self, ctx: &mut Context, doc: &Document) {
        if self.counted(doc) {
            self.update(ctx);
        }
    }

    fn active_editor_changed(&mut self, ctx: &mut Context, _editor: Option<&TextEditor>) {
        self.update(ctx);
    }

    fn selection_changed(&mut self, ctx: &mut Context, editor: &TextEditor) {
        if self.counted(&editor.document) {
            self.update(ctx);
        }
    }

    fn configuration_changed(&mut self, ctx: &mut Context) {
        self.read_settings(ctx);
        self.update(ctx);
    }
}

fn main() {
    orbvane_extension::run::<WordCount>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbvane_extension::Range;

    #[test]
    fn counts_words_and_selections() {
        assert_eq!(count("Hello, world!\n\nA *test* -- here.\n", &[]), Counts { words: 5, characters: 33, lines: 3 });
        assert_eq!(count("the cat and the dog", &["the".into()]).words, 3);
        let editor = TextEditor {
            document: Document::default(),
            selections: vec![Range::new(Position::new(0, 6), Position::new(1, 3))],
        };
        assert_eq!(selected_text(&editor, "Hello world\nfoo bar"), Some("world\nfoo".into()));
        let none = TextEditor { document: Document::default(), selections: vec![Range::default()] };
        assert_eq!(selected_text(&none, "x"), None);
    }
}
