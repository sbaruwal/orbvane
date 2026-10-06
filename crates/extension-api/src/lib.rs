//! Orbvane extensions in Rust.
//!
//! An extension is a folder with a `package.json` and, for code, a program named by
//! `main` (or `orbvane.main`). The editor starts the program when one of its activation events
//! happens (`onCommand:<id>`, implicit for contributed commands; `onLanguage:<id>`;
//! `workspaceContains:<glob>`; `onStartupFinished`; `*`) and talks to it over stdin/stdout.
//!
//! ```no_run
//! use orbvane_extension::{Context, Extension};
//! use serde_json::Value;
//!
//! struct Hello;
//!
//! impl Extension for Hello {
//!     fn activate(_ctx: &mut Context) -> Self {
//!         Hello
//!     }
//!
//!     fn command(&mut self, ctx: &mut Context, command: &str, _args: &[Value]) -> Result<Value, String> {
//!         match command {
//!             "hello.world" => {
//!                 ctx.show_information_message("Hello World from Rust!");
//!                 Ok(Value::Null)
//!             }
//!             _ => Err(format!("unknown command {command}")),
//!         }
//!     }
//! }
//!
//! fn main() {
//!     orbvane_extension::run::<Hello>();
//! }
//! ```
//!
//! # Protocol
//!
//! JSON-RPC 2.0 with LSP's `Content-Length` framing. Positions are `{ "line", "character" }`,
//! 0-based, with `character` in UTF-8 bytes of the line (so `&line[..character]` works).
//!
//! The editor sends requests `initialize` (`extension`, `workspaceFolders`, `activationEvent`),
//! `executeCommand` (`command`, `arguments`), `treeView/getChildren` (`viewId`, `element`: an
//! item id or null for the roots; answered with tree items), `provideHover`,
//! `provideCompletionItems` (with `triggerCharacter`) and `provideDefinition` (`provider`,
//! `document` with its text, `position`), `provideCodeActions` (`provider`, `document`,
//! `range`, `context`: `{ "diagnostics" }`; answered with LSP `CodeAction`s whose `command` is
//! `{ "command", "arguments" }`), `provideDocumentFormattingEdits` (`provider`, `document`,
//! `options`: `{ "tabSize", "insertSpaces" }`, `range` or null; answered with `TextEdit`s) and
//! `shutdown`, and notifications `exit`,
//! `didOpenTextDocument`, `didChangeTextDocument`, `didSaveTextDocument`, `didCloseTextDocument`
//! (`document`), `didChangeActiveTextEditor` (`editor` or null), `didChangeTextEditorSelection`
//! (`editor`), `didChangeConfiguration` and `didChangeWorkspaceFolders` (`folders`).
//!
//! The program sends notifications `window/showMessage` (`type` 1 error, 2 warning, 3 info,
//! `message`), `window/logMessage` (to the extension's output channel), `window/setStatusBarItem`,
//! `window/removeStatusBarItem`, `output/append`, `output/show`, `output/clear` and
//! `commands/register`, and requests `window/showMessageRequest` (`actions`: `[{ "title" }]`),
//! `window/showQuickPick`, `window/showInputBox`, `window/showTextDocument`,
//! `window/activeTextEditor`, `workspace/textDocument`, `workspace/applyEdit` (an LSP
//! `WorkspaceEdit`), `workspace/configuration`, `workspace/updateConfiguration` and
//! `commands/execute`.
//!
//! Views, decorations and language features (notifications from the program):
//! `treeView/refresh` (`viewId`, `element`) for the views the manifest contributes
//! (`contributes.views`, in the Explorer or a `viewsContainers.activitybar` container);
//! `window/createTextEditorDecorationType` (`key`, `options`), `window/setDecorations` (`key`,
//! `path`, `decorations`: `[{ "range", "hoverMessage" }]`) and `window/disposeDecorationType`;
//! `languages/registerProvider` (`id`, `kind`: `hover`, `completion`, `definition`,
//! `codeAction` or `formatting`, `selector`:
//! language ids, `"*"` or `{ "language", "pattern" }`, `triggerCharacters`);
//! `languages/setDiagnostics` (`collection`, `path`, `diagnostics`: `[{ "range", "severity"` 1
//! error .. 4 hint`, "message", "source", "code" }]`) and `languages/clearDiagnostics`
//! (`collection`). Colors are `"#rrggbb[aa]"` or a theme color `{ "id": "editorWarning.foreground" }`.

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

/// A position in a document: 0-based line, and `character` in UTF-8 bytes of the line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Position { line, character }
    }

    fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }

    fn from_json(v: &Value) -> Self {
        Position { line: v["line"].as_u64().unwrap_or(0) as u32, character: v["character"].as_u64().unwrap_or(0) as u32 }
    }
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Range { start, end }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn to_json(self) -> Value {
        json!({ "start": self.start.to_json(), "end": self.end.to_json() })
    }

    fn from_json(v: &Value) -> Self {
        Range { start: Position::from_json(&v["start"]), end: Position::from_json(&v["end"]) }
    }
}

/// An open document (or a file, for `Context::document`). Events don't carry the text; ask
/// for it with `Context::text`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    /// None for an untitled document.
    pub path: Option<PathBuf>,
    /// The editor's name for untitled documents ("Untitled-1"), else the file name.
    pub name: String,
    /// The language id ("rust", "markdown").
    pub language_id: String,
    /// Grows with every edit.
    pub version: u64,
    pub is_dirty: bool,
    /// Set by `Context::document` and `Context::active_text_editor`.
    pub text: Option<String>,
}

impl Document {
    fn from_json(v: &Value) -> Self {
        Document {
            path: v["path"].as_str().map(PathBuf::from),
            name: v["name"].as_str().unwrap_or("").to_string(),
            language_id: v["languageId"].as_str().unwrap_or("").to_string(),
            version: v["version"].as_u64().unwrap_or(0),
            is_dirty: v["isDirty"].as_bool().unwrap_or(false),
            text: v["text"].as_str().map(String::from),
        }
    }
}

/// The active editor: its document and selections (the primary one first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextEditor {
    pub document: Document,
    pub selections: Vec<Range>,
}

impl TextEditor {
    fn from_json(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        Some(TextEditor {
            document: Document::from_json(&v["document"]),
            selections: v["selections"].as_array().into_iter().flatten().map(Range::from_json).collect(),
        })
    }

    /// The primary selection.
    pub fn selection(&self) -> Range {
        self.selections.first().copied().unwrap_or_default()
    }
}

/// A replacement in a document.
#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    fn to_json(&self) -> Value {
        json!({ "range": self.range.to_json(), "newText": self.new_text })
    }

    pub fn replace(range: Range, new_text: impl Into<String>) -> Self {
        TextEdit { range, new_text: new_text.into() }
    }

    pub fn insert(at: Position, text: impl Into<String>) -> Self {
        TextEdit { range: Range::new(at, at), new_text: text.into() }
    }
}

/// Edits to several files, applied as one step each (`Context::apply_edit`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceEdit {
    pub changes: Vec<(PathBuf, Vec<TextEdit>)>,
}

impl WorkspaceEdit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, path: impl Into<PathBuf>, range: Range, new_text: impl Into<String>) -> &mut Self {
        let path = path.into();
        let edit = TextEdit::replace(range, new_text);
        match self.changes.iter_mut().find(|(p, _)| *p == path) {
            Some((_, edits)) => edits.push(edit),
            None => self.changes.push((path, vec![edit])),
        }
        self
    }

    pub fn insert(&mut self, path: impl Into<PathBuf>, at: Position, text: impl Into<String>) -> &mut Self {
        self.replace(path, Range::new(at, at), text)
    }

    fn to_json(&self) -> Value {
        let mut changes = serde_json::Map::new();
        for (path, edits) in &self.changes {
            let edits: Vec<Value> = edits.iter().map(TextEdit::to_json).collect();
            changes.insert(file_uri(path), edits.into());
        }
        json!({ "changes": changes })
    }
}

fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Error = 1,
    Warning = 2,
    Info = 3,
}

/// An entry of `Context::show_quick_pick`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuickPickItem {
    pub label: String,
    /// Shown beside the label, dimmed.
    pub description: String,
    /// Shown under the label.
    pub detail: String,
}

impl QuickPickItem {
    pub fn new(label: impl Into<String>) -> Self {
        QuickPickItem { label: label.into(), ..Default::default() }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputBoxOptions {
    pub title: String,
    /// The message under the input.
    pub prompt: String,
    pub placeholder: String,
    /// The initial value.
    pub value: String,
    /// Show dots instead of the text.
    pub password: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Alignment {
    #[default]
    Left,
    Right,
}

/// An entry in the status bar. Setting one with the same `id` again updates it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusBarItem {
    pub id: String,
    /// May contain icons as `$(name)` (`$(check)`, `$(sync~spin)`...).
    pub text: String,
    pub tooltip: String,
    /// A command id run when it's clicked.
    pub command: Option<String>,
    pub alignment: Alignment,
    /// Higher is further left.
    pub priority: i32,
}

/// A command with arguments, run when something is clicked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandCall {
    pub command: String,
    pub arguments: Vec<Value>,
}

impl CommandCall {
    pub fn new(command: impl Into<String>, arguments: Vec<Value>) -> Self {
        CommandCall { command: command.into(), arguments }
    }

    fn to_json(&self) -> Value {
        json!({ "command": self.command, "arguments": self.arguments })
    }
}

/// Whether a tree item has children, and whether they show at first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Collapsible {
    #[default]
    None = 0,
    Collapsed = 1,
    Expanded = 2,
}

/// An entry of a tree view (`Extension::tree_children`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeItem {
    /// Unique in the view; the editor asks for this item's children by it.
    pub id: String,
    pub label: String,
    /// Shown beside the label, dimmed.
    pub description: String,
    pub tooltip: String,
    /// An icon name (`$(file)`, `$(warning)`) or an image in the extension's folder.
    pub icon: Option<String>,
    pub collapsible: Collapsible,
    /// Run when the item is clicked.
    pub command: Option<CommandCall>,
    /// Matched by `viewItem == <value>` in the `view/item/context` menus' `when`.
    pub context_value: Option<String>,
}

impl TreeItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        TreeItem { id: id.into(), label: label.into(), ..Default::default() }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    pub fn tooltip(mut self, t: impl Into<String>) -> Self {
        self.tooltip = t.into();
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn collapsible(mut self, c: Collapsible) -> Self {
        self.collapsible = c;
        self
    }

    pub fn command(mut self, command: CommandCall) -> Self {
        self.command = Some(command);
        self
    }

    pub fn context_value(mut self, v: impl Into<String>) -> Self {
        self.context_value = Some(v.into());
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "description": self.description,
            "tooltip": self.tooltip,
            "icon": self.icon,
            "collapsibleState": self.collapsible as u8,
            "command": self.command.as_ref().map(CommandCall::to_json),
            "contextValue": self.context_value,
        })
    }
}

/// A color: a theme color (`Color::theme("editorWarning.foreground")`, follows the theme) or a
/// fixed one (`Color::hex("#ff000040")`).
#[derive(Clone, Debug, PartialEq)]
pub enum Color {
    Theme(String),
    Hex(String),
}

impl Color {
    pub fn theme(id: impl Into<String>) -> Self {
        Color::Theme(id.into())
    }

    pub fn hex(hex: impl Into<String>) -> Self {
        Color::Hex(hex.into())
    }

    fn to_json(&self) -> Value {
        match self {
            Color::Theme(id) => json!({ "id": id }),
            Color::Hex(h) => json!(h),
        }
    }
}

/// Text shown before or after a decorated range.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attachment {
    pub content_text: String,
    pub color: Option<Color>,
    pub font_style: Option<String>,
}

/// How a decoration type draws.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecorationOptions {
    pub background_color: Option<Color>,
    /// The text's color.
    pub color: Option<Color>,
    pub border_color: Option<Color>,
    /// `"bold"`.
    pub font_weight: Option<String>,
    /// `"italic"`.
    pub font_style: Option<String>,
    /// `"underline"`, `"line-through"` or `"underline wavy"`.
    pub text_decoration: Option<String>,
    /// The background covers whole lines.
    pub is_whole_line: bool,
    /// A mark in the scroll bar.
    pub overview_ruler_color: Option<Color>,
    pub before: Option<Attachment>,
    pub after: Option<Attachment>,
}

fn attachment_json(a: &Option<Attachment>) -> Value {
    match a {
        None => Value::Null,
        Some(a) => json!({ "contentText": a.content_text, "color": a.color.as_ref().map(Color::to_json), "fontStyle": a.font_style }),
    }
}

impl DecorationOptions {
    fn to_json(&self) -> Value {
        json!({
            "backgroundColor": self.background_color.as_ref().map(Color::to_json),
            "color": self.color.as_ref().map(Color::to_json),
            "borderColor": self.border_color.as_ref().map(Color::to_json),
            "fontWeight": self.font_weight,
            "fontStyle": self.font_style,
            "textDecoration": self.text_decoration,
            "isWholeLine": self.is_whole_line,
            "overviewRulerColor": self.overview_ruler_color.as_ref().map(Color::to_json),
            "before": attachment_json(&self.before),
            "after": attachment_json(&self.after),
        })
    }
}

/// A decorated range, with an optional hover (Markdown).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decoration {
    pub range: Range,
    pub hover_message: Option<String>,
}

impl Decoration {
    pub fn new(range: Range) -> Self {
        Decoration { range, hover_message: None }
    }

    pub fn hover(mut self, markdown: impl Into<String>) -> Self {
        self.hover_message = Some(markdown.into());
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    #[default]
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

/// A problem in a file (squiggles, the Problems panel).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// Who found it (shown after the message).
    pub source: Option<String>,
    pub code: Option<String>,
}

impl Diagnostic {
    pub fn new(range: Range, severity: DiagnosticSeverity, message: impl Into<String>) -> Self {
        Diagnostic { range, severity, message: message.into(), source: None, code: None }
    }

    pub fn source(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }

    pub fn code(mut self, c: impl Into<String>) -> Self {
        self.code = Some(c.into());
        self
    }

    fn to_json(&self) -> Value {
        json!({ "range": self.range.to_json(), "severity": self.severity as u8, "message": self.message, "source": self.source, "code": self.code })
    }

    fn from_json(v: &Value) -> Self {
        let severity = match v["severity"].as_u64() {
            Some(2) => DiagnosticSeverity::Warning,
            Some(3) => DiagnosticSeverity::Information,
            Some(4) => DiagnosticSeverity::Hint,
            _ => DiagnosticSeverity::Error,
        };
        let code = match &v["code"] {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Diagnostic { range: Range::from_json(&v["range"]), severity, message: v["message"].as_str().unwrap_or("").to_string(), source: v["source"].as_str().map(String::from), code }
    }
}

/// A fix or refactoring offered at a range (`Extension::provide_code_actions`): its edits are
/// applied, then its command runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeAction {
    pub title: String,
    /// `"quickfix"`, `"refactor"`, `"refactor.extract"`, `"source"`... (groups the menu; quick
    /// fixes first).
    pub kind: Option<String>,
    pub edit: Option<WorkspaceEdit>,
    pub command: Option<CommandCall>,
    /// The fix to pick when there are several.
    pub is_preferred: bool,
    /// The problems it fixes.
    pub diagnostics: Vec<Diagnostic>,
}

impl CodeAction {
    pub fn quick_fix(title: impl Into<String>, edit: WorkspaceEdit) -> Self {
        CodeAction { title: title.into(), kind: Some("quickfix".into()), edit: Some(edit), ..Default::default() }
    }

    fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "kind": self.kind,
            "edit": self.edit.as_ref().map(WorkspaceEdit::to_json),
            "command": self.command.as_ref().map(|c| json!({ "title": self.title, "command": c.command, "arguments": c.arguments })),
            "isPreferred": self.is_preferred,
            "diagnostics": self.diagnostics.iter().map(Diagnostic::to_json).collect::<Vec<_>>(),
        })
    }
}

/// How the user indents (`Extension::provide_formatting`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormattingOptions {
    pub tab_size: u32,
    pub insert_spaces: bool,
}

/// What a hover provider shows (Markdown).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hover {
    pub contents: String,
    /// The range the hover is about (default: the word under the pointer).
    pub range: Option<Range>,
}

impl Hover {
    pub fn new(markdown: impl Into<String>) -> Self {
        Hover { contents: markdown.into(), range: None }
    }
}

/// LSP's completion item kinds (the icon in the suggest widget).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionItemKind {
    #[default]
    Text = 1,
    Method = 2,
    Function = 3,
    Constructor = 4,
    Field = 5,
    Variable = 6,
    Class = 7,
    Interface = 8,
    Module = 9,
    Property = 10,
    Unit = 11,
    Value = 12,
    Enum = 13,
    Keyword = 14,
    Snippet = 15,
    Color = 16,
    File = 17,
    Reference = 18,
    Folder = 19,
    EnumMember = 20,
    Constant = 21,
    Struct = 22,
    Event = 23,
    Operator = 24,
    TypeParameter = 25,
}

/// A suggestion (`Extension::provide_completion`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionItemKind,
    pub detail: String,
    /// Markdown.
    pub documentation: String,
    /// What's inserted (default: the label), replacing the word before the cursor.
    pub insert_text: Option<String>,
    /// `insert_text` is a snippet (`${1:name}`, `$0`).
    pub snippet: bool,
    pub sort_text: Option<String>,
    pub filter_text: Option<String>,
}

impl CompletionItem {
    pub fn new(label: impl Into<String>, kind: CompletionItemKind) -> Self {
        CompletionItem { label: label.into(), kind, ..Default::default() }
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }

    pub fn documentation(mut self, d: impl Into<String>) -> Self {
        self.documentation = d.into();
        self
    }

    pub fn insert_text(mut self, t: impl Into<String>) -> Self {
        self.insert_text = Some(t.into());
        self
    }

    pub fn snippet(mut self, s: impl Into<String>) -> Self {
        self.insert_text = Some(s.into());
        self.snippet = true;
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "kind": self.kind as u8,
            "detail": self.detail,
            "documentation": { "kind": "markdown", "value": self.documentation },
            "insertText": self.insert_text,
            "insertTextFormat": if self.snippet { 2 } else { 1 },
            "sortText": self.sort_text,
            "filterText": self.filter_text,
        })
    }
}

/// A place in a file (`Extension::provide_definition`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

impl Location {
    pub fn new(path: impl Into<PathBuf>, range: Range) -> Self {
        Location { path: path.into(), range }
    }

    fn to_json(&self) -> Value {
        json!({ "uri": file_uri(&self.path), "range": self.range.to_json() })
    }
}

/// Which documents a provider is for: language ids (`"rust"`), `"*"` for all, or a glob on the
/// path (`**/*.todo`).
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentFilter {
    Language(String),
    Pattern(String),
}

impl DocumentFilter {
    fn to_json(&self) -> Value {
        match self {
            DocumentFilter::Language(l) => json!(l),
            DocumentFilter::Pattern(p) => json!({ "pattern": p }),
        }
    }
}

impl From<&str> for DocumentFilter {
    fn from(s: &str) -> Self {
        DocumentFilter::Language(s.to_string())
    }
}

/// What a running extension can ask of the editor. Requests block until the editor answers
/// (the user picks an item, types a value...); messages the editor sends meanwhile are handled
/// after the current call returns.
pub struct Context {
    out: Box<dyn Write>,
    input: Box<dyn BufRead>,
    next_id: i64,
    /// Messages that arrived while waiting for an answer.
    backlog: VecDeque<Value>,
    /// The extension's folder.
    pub extension_path: PathBuf,
    /// The extension's `publisher.name`.
    pub extension_id: String,
    pub workspace_folders: Vec<PathBuf>,
}

impl Context {
    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        let _ = write!(self.out, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = self.out.flush();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request and waits for its answer (an error message on failure).
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let msg = read_message(&mut self.input).ok_or("the editor closed the connection")?;
            if msg.get("method").is_none() && msg["id"].as_i64() == Some(id) {
                return match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(msg["result"].clone()),
                };
            }
            self.backlog.push_back(msg);
        }
    }

    pub fn show_message(&mut self, kind: MessageType, message: &str) {
        self.notify("window/showMessage", json!({ "type": kind as u8, "message": message }));
    }

    pub fn show_information_message(&mut self, message: &str) {
        self.show_message(MessageType::Info, message);
    }

    pub fn show_warning_message(&mut self, message: &str) {
        self.show_message(MessageType::Warning, message);
    }

    pub fn show_error_message(&mut self, message: &str) {
        self.show_message(MessageType::Error, message);
    }

    /// A notification with buttons; returns the one clicked (None when it's closed).
    pub fn show_message_request(&mut self, kind: MessageType, message: &str, actions: &[&str]) -> Option<String> {
        let actions: Vec<Value> = actions.iter().map(|a| json!({ "title": a })).collect();
        let answer = self.request("window/showMessageRequest", json!({ "type": kind as u8, "message": message, "actions": actions })).ok()?;
        answer["title"].as_str().map(String::from)
    }

    /// Lets the user pick one of `items` (filtered as they type); returns its index.
    pub fn show_quick_pick(&mut self, items: &[QuickPickItem], placeholder: &str) -> Option<usize> {
        let items: Vec<Value> = items.iter().map(|i| json!({ "label": i.label, "description": i.description, "detail": i.detail })).collect();
        self.request("window/showQuickPick", json!({ "items": items, "placeHolder": placeholder })).ok()?.as_u64().map(|i| i as usize)
    }

    /// Asks for a line of text; None when cancelled.
    pub fn show_input_box(&mut self, options: &InputBoxOptions) -> Option<String> {
        let params = json!({ "title": options.title, "prompt": options.prompt, "placeHolder": options.placeholder, "value": options.value, "password": options.password });
        self.request("window/showInputBox", params).ok()?.as_str().map(String::from)
    }

    pub fn set_status_bar_item(&mut self, item: &StatusBarItem) {
        let alignment = if item.alignment == Alignment::Right { "right" } else { "left" };
        let params = json!({ "id": item.id, "text": item.text, "tooltip": item.tooltip, "command": item.command, "alignment": alignment, "priority": item.priority });
        self.notify("window/setStatusBarItem", params);
    }

    pub fn remove_status_bar_item(&mut self, id: &str) {
        self.notify("window/removeStatusBarItem", json!({ "id": id }));
    }

    /// Appends text to an output channel (made on first use) in the Output panel.
    pub fn append_output(&mut self, channel: &str, text: &str) {
        self.notify("output/append", json!({ "channel": channel, "text": text }));
    }

    pub fn append_output_line(&mut self, channel: &str, line: &str) {
        self.append_output(channel, &format!("{line}\n"));
    }

    /// Shows an output channel in the Output panel.
    pub fn show_output(&mut self, channel: &str, preserve_focus: bool) {
        self.notify("output/show", json!({ "channel": channel, "preserveFocus": preserve_focus }));
    }

    pub fn clear_output(&mut self, channel: &str) {
        self.notify("output/clear", json!({ "channel": channel }));
    }

    /// Writes to the extension's own output channel (named after it).
    pub fn log(&mut self, message: &str) {
        self.notify("window/logMessage", json!({ "type": MessageType::Info as u8, "message": message }));
    }

    /// Declares a command the manifest doesn't list (it won't be in the command palette).
    pub fn register_command(&mut self, command: &str) {
        self.notify("commands/register", json!({ "command": command }));
    }

    /// Runs a command: the editor's or another
    /// extension's. Don't run this extension's own commands this way; call your code directly.
    pub fn execute_command(&mut self, command: &str, args: &[Value]) -> Result<Value, String> {
        self.request("commands/execute", json!({ "command": command, "arguments": args }))
    }

    /// The active text editor, with its document's text.
    pub fn active_text_editor(&mut self) -> Option<TextEditor> {
        TextEditor::from_json(&self.request("window/activeTextEditor", Value::Null).ok()?)
    }

    /// A document with its text: the open one, else the file on disk.
    pub fn document(&mut self, path: &std::path::Path) -> Option<Document> {
        let v = self.request("workspace/textDocument", json!({ "path": path })).ok()?;
        v.is_object().then(|| Document::from_json(&v))
    }

    /// A document's current text (`doc.text` if it has it).
    pub fn text(&mut self, doc: &Document) -> Option<String> {
        if let Some(t) = &doc.text {
            return Some(t.clone());
        }
        let path = doc.path.clone()?;
        self.document(&path)?.text
    }

    /// Opens a file in an editor, selecting `selection` if given.
    pub fn show_text_document(&mut self, path: &std::path::Path, selection: Option<Range>) -> bool {
        let params = json!({ "path": path, "selection": selection.map(Range::to_json) });
        self.request("window/showTextDocument", params).is_ok()
    }

    /// Applies edits (to open documents, or files on disk); false if any couldn't be applied.
    pub fn apply_edit(&mut self, edit: &WorkspaceEdit) -> bool {
        self.request("workspace/applyEdit", json!({ "edit": edit.to_json() })).ok().and_then(|v| v["applied"].as_bool()).unwrap_or(false)
    }

    /// A setting's effective value (`wordCount.mode`), or an object of the settings under a
    /// section (`wordCount`).
    pub fn configuration(&mut self, key: &str) -> Value {
        self.request("workspace/configuration", json!({ "key": key })).unwrap_or(Value::Null)
    }

    /// Changes a setting in the user's settings (or the workspace's); `None` removes it.
    pub fn update_configuration(&mut self, key: &str, value: Option<Value>, workspace: bool) -> Result<(), String> {
        let target = if workspace { "workspace" } else { "user" };
        self.request("workspace/updateConfiguration", json!({ "key": key, "value": value, "target": target })).map(|_| ())
    }

    /// Asks the editor to fetch a tree view's items again: all of them, or `element`'s children.
    pub fn refresh_tree(&mut self, view: &str, element: Option<&str>) {
        self.notify("treeView/refresh", json!({ "viewId": view, "element": element }));
    }

    /// Defines how decorations of type `key` look (calling it again replaces the look).
    pub fn create_decoration_type(&mut self, key: &str, options: &DecorationOptions) {
        self.notify("window/createTextEditorDecorationType", json!({ "key": key, "options": options.to_json() }));
    }

    /// Sets the decorations of type `key` in a file (replacing the ones it had); they move with
    /// edits until set again.
    pub fn set_decorations(&mut self, key: &str, path: &std::path::Path, decorations: &[Decoration]) {
        let decorations: Vec<Value> = decorations.iter().map(|d| json!({ "range": d.range.to_json(), "hoverMessage": d.hover_message })).collect();
        self.notify("window/setDecorations", json!({ "key": key, "path": path, "decorations": decorations }));
    }

    /// Removes a decoration type and all its decorations.
    pub fn dispose_decoration_type(&mut self, key: &str) {
        self.notify("window/disposeDecorationType", json!({ "key": key }));
    }

    fn register_provider(&mut self, id: &str, kind: &str, selector: &[DocumentFilter], triggers: &[&str]) {
        let selector: Vec<Value> = selector.iter().map(DocumentFilter::to_json).collect();
        self.notify("languages/registerProvider", json!({ "id": id, "kind": kind, "selector": selector, "triggerCharacters": triggers }));
    }

    /// Hovers in matching documents come from `Extension::provide_hover` (with `id`).
    pub fn register_hover_provider(&mut self, id: &str, selector: &[DocumentFilter]) {
        self.register_provider(id, "hover", selector, &[]);
    }

    /// Suggestions in matching documents come from `Extension::provide_completion`; typing one of
    /// `triggers` asks too.
    pub fn register_completion_provider(&mut self, id: &str, selector: &[DocumentFilter], triggers: &[&str]) {
        self.register_provider(id, "completion", selector, triggers);
    }

    /// Go to Definition in matching documents asks `Extension::provide_definition`.
    pub fn register_definition_provider(&mut self, id: &str, selector: &[DocumentFilter]) {
        self.register_provider(id, "definition", selector, &[]);
    }

    /// Quick Fix and the lightbulb in matching documents ask `Extension::provide_code_actions`.
    pub fn register_code_action_provider(&mut self, id: &str, selector: &[DocumentFilter]) {
        self.register_provider(id, "codeAction", selector, &[]);
    }

    /// Format Document (and format on save) in matching documents asks
    /// `Extension::provide_formatting` when no language server formats them.
    pub fn register_formatting_provider(&mut self, id: &str, selector: &[DocumentFilter]) {
        self.register_provider(id, "formatting", selector, &[]);
    }

    /// Sets a file's problems in a collection (an empty list clears them).
    pub fn set_diagnostics(&mut self, collection: &str, path: &std::path::Path, diagnostics: &[Diagnostic]) {
        let diagnostics: Vec<Value> = diagnostics.iter().map(Diagnostic::to_json).collect();
        self.notify("languages/setDiagnostics", json!({ "collection": collection, "path": path, "diagnostics": diagnostics }));
    }

    /// Removes every problem in a collection.
    pub fn clear_diagnostics(&mut self, collection: &str) {
        self.notify("languages/clearDiagnostics", json!({ "collection": collection }));
    }
}

/// An extension's code. `activate` runs when the editor starts the program; the rest are
/// called as things happen.
pub trait Extension: Sized {
    fn activate(ctx: &mut Context) -> Self;

    /// Runs one of the extension's commands. The result goes back to whoever ran it.
    fn command(&mut self, ctx: &mut Context, command: &str, args: &[Value]) -> Result<Value, String> {
        let _ = (ctx, args);
        Err(format!("command '{command}' not found"))
    }

    fn did_open(&mut self, _ctx: &mut Context, _doc: &Document) {}
    fn did_change(&mut self, _ctx: &mut Context, _doc: &Document) {}
    fn did_save(&mut self, _ctx: &mut Context, _doc: &Document) {}
    fn did_close(&mut self, _ctx: &mut Context, _doc: &Document) {}
    fn active_editor_changed(&mut self, _ctx: &mut Context, _editor: Option<&TextEditor>) {}
    fn selection_changed(&mut self, _ctx: &mut Context, _editor: &TextEditor) {}
    fn configuration_changed(&mut self, _ctx: &mut Context) {}
    fn workspace_folders_changed(&mut self, _ctx: &mut Context) {}

    /// The items of tree view `view` (from `contributes.views`): the roots when `element` is
    /// None, else that item's children.
    fn tree_children(&mut self, _ctx: &mut Context, view: &str, _element: Option<&str>) -> Result<Vec<TreeItem>, String> {
        Err(format!("no data for view '{view}'"))
    }

    /// The hover at `pos` (for providers registered with `register_hover_provider`).
    fn provide_hover(&mut self, _ctx: &mut Context, _provider: &str, _doc: &Document, _pos: Position) -> Option<Hover> {
        None
    }

    /// Suggestions at `pos`; `trigger` is the character typed, if one of the provider's triggers.
    fn provide_completion(&mut self, _ctx: &mut Context, _provider: &str, _doc: &Document, _pos: Position, _trigger: Option<&str>) -> Vec<CompletionItem> {
        Vec::new()
    }

    /// Where the symbol at `pos` is defined.
    fn provide_definition(&mut self, _ctx: &mut Context, _provider: &str, _doc: &Document, _pos: Position) -> Vec<Location> {
        Vec::new()
    }

    /// The fixes and refactorings for `range`; `diagnostics` are the problems there.
    fn provide_code_actions(&mut self, _ctx: &mut Context, _provider: &str, _doc: &Document, _range: Range, _diagnostics: &[Diagnostic]) -> Vec<CodeAction> {
        Vec::new()
    }

    /// The edits that format the document (or `range` of it, for Format Selection).
    fn provide_formatting(&mut self, _ctx: &mut Context, _provider: &str, _doc: &Document, _options: FormattingOptions, _range: Option<Range>) -> Vec<TextEdit> {
        Vec::new()
    }

    /// The editor is stopping the extension.
    fn deactivate(&mut self, _ctx: &mut Context) {}
}

/// Runs the extension over stdin/stdout until the editor stops it. Call it from `main`.
pub fn run<E: Extension>() {
    let stdin = io::stdin();
    run_with::<E>(Box::new(io::BufReader::new(stdin)), Box::new(io::stdout()));
}

/// Runs the extension over the given streams (for tests).
pub fn run_with<E: Extension>(input: Box<dyn BufRead>, out: Box<dyn Write>) {
    let mut ctx = Context { out, input, next_id: 1, backlog: VecDeque::new(), extension_path: PathBuf::new(), extension_id: String::new(), workspace_folders: Vec::new() };
    let mut ext: Option<E> = None;
    loop {
        let msg = match ctx.backlog.pop_front() {
            Some(m) => m,
            None => match read_message(&mut ctx.input) {
                Some(m) => m,
                None => break,
            },
        };
        let Some(method) = msg["method"].as_str().map(String::from) else { continue };
        let params = &msg["params"];
        let id = msg.get("id").cloned();
        let reply = |ctx: &mut Context, result: Result<Value, String>| {
            if let Some(id) = &id {
                let msg = match result {
                    Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
                    Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e } }),
                };
                ctx.send(msg);
            }
        };
        match method.as_str() {
            "initialize" => {
                ctx.extension_path = params["extension"]["path"].as_str().map(PathBuf::from).unwrap_or_default();
                ctx.extension_id = params["extension"]["id"].as_str().unwrap_or("").to_string();
                ctx.workspace_folders = folders(&params["workspaceFolders"]);
                reply(&mut ctx, Ok(json!({})));
                ext = Some(E::activate(&mut ctx));
            }
            "shutdown" => {
                if let Some(e) = &mut ext {
                    e.deactivate(&mut ctx);
                }
                reply(&mut ctx, Ok(Value::Null));
            }
            "exit" => break,
            _ => {
                let Some(e) = &mut ext else {
                    reply(&mut ctx, Err("not initialized".into()));
                    continue;
                };
                let doc = || Document::from_json(&params["document"]);
                match method.as_str() {
                    "executeCommand" => {
                        let args: Vec<Value> = params["arguments"].as_array().cloned().unwrap_or_default();
                        let command = params["command"].as_str().unwrap_or("").to_string();
                        let result = e.command(&mut ctx, &command, &args);
                        reply(&mut ctx, result);
                    }
                    "treeView/getChildren" => {
                        let view = params["viewId"].as_str().unwrap_or("").to_string();
                        let element = params["element"].as_str().map(String::from);
                        let result = e.tree_children(&mut ctx, &view, element.as_deref()).map(|items| Value::Array(items.iter().map(TreeItem::to_json).collect()));
                        reply(&mut ctx, result);
                    }
                    "provideHover" | "provideCompletionItems" | "provideDefinition" => {
                        let provider = params["provider"].as_str().unwrap_or("").to_string();
                        let (d, pos) = (doc(), Position::from_json(&params["position"]));
                        let result = match method.as_str() {
                            "provideHover" => match e.provide_hover(&mut ctx, &provider, &d, pos) {
                                Some(h) => json!({ "contents": { "kind": "markdown", "value": h.contents }, "range": h.range.map(Range::to_json) }),
                                None => Value::Null,
                            },
                            "provideCompletionItems" => {
                                let trigger = params["triggerCharacter"].as_str().map(String::from);
                                Value::Array(e.provide_completion(&mut ctx, &provider, &d, pos, trigger.as_deref()).iter().map(CompletionItem::to_json).collect())
                            }
                            _ => Value::Array(e.provide_definition(&mut ctx, &provider, &d, pos).iter().map(Location::to_json).collect()),
                        };
                        reply(&mut ctx, Ok(result));
                    }
                    "provideCodeActions" => {
                        let provider = params["provider"].as_str().unwrap_or("").to_string();
                        let diagnostics: Vec<Diagnostic> = params["context"]["diagnostics"].as_array().into_iter().flatten().map(Diagnostic::from_json).collect();
                        let actions = e.provide_code_actions(&mut ctx, &provider, &doc(), Range::from_json(&params["range"]), &diagnostics);
                        reply(&mut ctx, Ok(Value::Array(actions.iter().map(CodeAction::to_json).collect())));
                    }
                    "provideDocumentFormattingEdits" => {
                        let provider = params["provider"].as_str().unwrap_or("").to_string();
                        let o = &params["options"];
                        let options = FormattingOptions { tab_size: o["tabSize"].as_u64().unwrap_or(4) as u32, insert_spaces: o["insertSpaces"].as_bool().unwrap_or(true) };
                        let range = params["range"].is_object().then(|| Range::from_json(&params["range"]));
                        let edits = e.provide_formatting(&mut ctx, &provider, &doc(), options, range);
                        reply(&mut ctx, Ok(Value::Array(edits.iter().map(TextEdit::to_json).collect())));
                    }
                    "didOpenTextDocument" => e.did_open(&mut ctx, &doc()),
                    "didChangeTextDocument" => e.did_change(&mut ctx, &doc()),
                    "didSaveTextDocument" => e.did_save(&mut ctx, &doc()),
                    "didCloseTextDocument" => e.did_close(&mut ctx, &doc()),
                    "didChangeActiveTextEditor" => e.active_editor_changed(&mut ctx, TextEditor::from_json(&params["editor"]).as_ref()),
                    "didChangeTextEditorSelection" => {
                        if let Some(ed) = TextEditor::from_json(&params["editor"]) {
                            e.selection_changed(&mut ctx, &ed);
                        }
                    }
                    "didChangeConfiguration" => e.configuration_changed(&mut ctx),
                    "didChangeWorkspaceFolders" => {
                        ctx.workspace_folders = folders(&params["folders"]);
                        e.workspace_folders_changed(&mut ctx);
                    }
                    _ => reply(&mut ctx, Err(format!("unknown method {method}"))),
                }
            }
        }
    }
}

fn folders(v: &Value) -> Vec<PathBuf> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from).collect()
}

/// Reads one `Content-Length`-framed message.
fn read_message(reader: &mut dyn BufRead) -> Option<Value> {
    let mut len = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let header = line.trim_end();
        if header.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some(v) = header.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn frame(v: Value) -> String {
        let body = v.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    /// Collects what the extension writes.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Counter {
        words: usize,
    }

    impl Extension for Counter {
        fn activate(ctx: &mut Context) -> Self {
            ctx.register_command("count.hidden");
            Counter { words: 0 }
        }

        fn command(&mut self, ctx: &mut Context, command: &str, _args: &[Value]) -> Result<Value, String> {
            match command {
                "count.words" => {
                    let editor = ctx.active_text_editor().ok_or("no editor")?;
                    self.words = editor.document.text.unwrap_or_default().split_whitespace().count();
                    ctx.show_information_message(&format!("{} words", self.words));
                    Ok(json!(self.words))
                }
                _ => Err(format!("command '{command}' not found")),
            }
        }
    }

    #[test]
    fn answers_the_editor() {
        let input = [
            frame(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "extension": { "id": "a.b", "path": "/ext" }, "workspaceFolders": ["/w"] } })),
            frame(json!({ "jsonrpc": "2.0", "id": 2, "method": "executeCommand", "params": { "command": "count.words", "arguments": [] } })),
            // The answer to `window/activeTextEditor`, then a notification that arrives meanwhile.
            frame(json!({ "jsonrpc": "2.0", "method": "didChangeConfiguration", "params": {} })),
            frame(json!({ "jsonrpc": "2.0", "id": 1, "result": { "document": { "path": "/w/a.md", "languageId": "markdown", "version": 3, "text": "one two  three" }, "selections": [] } })),
            frame(json!({ "jsonrpc": "2.0", "id": 3, "method": "executeCommand", "params": { "command": "nope" } })),
            frame(json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" })),
            frame(json!({ "jsonrpc": "2.0", "method": "exit" })),
        ]
        .concat();
        let sink = Sink::default();
        run_with::<Counter>(Box::new(io::Cursor::new(input.into_bytes())), Box::new(sink.clone()));
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let mut reader = io::Cursor::new(out.into_bytes());
        let msgs: Vec<Value> = std::iter::from_fn(|| read_message(&mut reader)).collect();
        assert_eq!(msgs[0]["id"], 1);
        assert_eq!(msgs[1]["method"], "commands/register");
        assert_eq!(msgs[2]["method"], "window/activeTextEditor");
        assert_eq!(msgs[3]["params"]["message"], "3 words");
        assert_eq!((msgs[4]["id"].clone(), msgs[4]["result"].clone()), (json!(2), json!(3)));
        assert_eq!(msgs[5]["error"]["message"], "command 'nope' not found");
        assert_eq!(msgs[6]["id"], 4);
        assert_eq!(msgs.len(), 7);
    }
}
