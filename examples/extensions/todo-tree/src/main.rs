//! TODO Tree: an example Orbvane extension showing the views and language features API. It
//! finds TODO and FIXME comments in the workspace and shows them in a tree view (its own
//! activity bar container, and the Explorer), highlights them in editors (decorations), reports
//! FIXMEs as problems (diagnostics), and adds a hover on tags, tag completions in comments and Go
//! to Definition on `path:line` references in TODO comments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use orbvane_extension::{
    Collapsible, Color, CommandCall, CompletionItem, CompletionItemKind, Context, Decoration, DecorationOptions, Diagnostic, DiagnosticSeverity, Document, Extension, Hover, Location, Position, Range, TreeItem,
    WorkspaceEdit,
};
use serde_json::{json, Value};

const COLLECTION: &str = "todo-tree";
const VIEWS: &[&str] = &["todoTree.todos", "todoTree.tags", "todoTree.explorer"];
/// Folders not worth looking in.
const SKIP: &[&str] = &["target", "node_modules", "build", "dist", ".git"];

/// A tag in a comment: `// TODO: text`.
#[derive(Clone, Debug, PartialEq)]
struct Todo {
    path: PathBuf,
    line: u32,
    /// Byte column of the tag.
    col: u32,
    tag: String,
    text: String,
}

impl Todo {
    fn id(&self) -> String {
        format!("todo:{}:{}", self.path.display(), self.line)
    }

    fn range(&self) -> Range {
        Range::new(Position::new(self.line, self.col), Position::new(self.line, self.col + self.tag.len() as u32))
    }
}

/// The tags in `text` (whole words followed by `:`, `(` or a space).
fn find_todos(path: &Path, text: &str, tags: &[String]) -> Vec<Todo> {
    let mut out = Vec::new();
    for (line, l) in text.lines().enumerate() {
        for tag in tags {
            let mut from = 0;
            while let Some(i) = l[from..].find(tag.as_str()) {
                let at = from + i;
                from = at + tag.len();
                let before = l[..at].chars().next_back();
                let after = l[from..].chars().next();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') || !matches!(after, None | Some(':' | '(' | ' ')) {
                    continue;
                }
                let rest = l[from..].trim_start_matches(|c: char| c == ':' || c == ' ');
                let rest = rest.split_once(')').filter(|_| l[from..].starts_with('(')).map_or(rest, |(_, r)| r.trim_start_matches([':', ' ']));
                out.push(Todo { path: path.to_path_buf(), line: line as u32, col: at as u32, tag: tag.clone(), text: rest.trim_end_matches("*/").trim_end_matches("-->").trim().to_string() });
            }
        }
    }
    out.sort_by_key(|t| (t.line, t.col));
    out
}

/// The text files under `dir` (skipping hidden and build folders).
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if name.starts_with('.') || SKIP.contains(&name.as_str()) || out.len() > 5000 {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else if e.metadata().is_ok_and(|m| m.len() < 1 << 20) {
            out.push(path);
        }
    }
}

struct TodoTree {
    tags: Vec<String>,
    /// The TODOs by file.
    files: BTreeMap<PathBuf, Vec<Todo>>,
}

impl TodoTree {
    fn read_tags(ctx: &mut Context) -> Vec<String> {
        let tags: Vec<String> = ctx.configuration("todoTree.tags").as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        if tags.is_empty() { vec!["TODO".into(), "FIXME".into()] } else { tags }
    }

    /// Looks through the workspace again and updates the views and problems.
    fn scan(&mut self, ctx: &mut Context) {
        let old: Vec<PathBuf> = self.files.keys().cloned().collect();
        self.files.clear();
        let mut paths = Vec::new();
        for folder in ctx.workspace_folders.clone() {
            walk(&folder, &mut paths);
        }
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let todos = find_todos(&path, &text, &self.tags);
            if !todos.is_empty() {
                self.files.insert(path, todos);
            }
        }
        for path in old.iter().filter(|p| !self.files.contains_key(*p)) {
            ctx.set_diagnostics(COLLECTION, path, &[]);
        }
        let paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in paths {
            self.report(ctx, &path);
        }
        for view in VIEWS {
            ctx.refresh_tree(view, None);
        }
    }

    /// FIXMEs as warnings.
    fn report(&self, ctx: &mut Context, path: &Path) {
        let diags: Vec<Diagnostic> = self
            .files
            .get(path)
            .into_iter()
            .flatten()
            .filter(|t| t.tag == "FIXME")
            .map(|t| Diagnostic::new(t.range(), DiagnosticSeverity::Warning, format!("FIXME: {}", t.text)).source("TODO Tree"))
            .collect();
        ctx.set_diagnostics(COLLECTION, path, &diags);
    }

    /// A document's text changed: its TODOs, problems and highlights.
    fn update(&mut self, ctx: &mut Context, doc: &Document) {
        let Some(path) = doc.path.clone() else { return };
        let Some(text) = ctx.text(doc) else { return };
        let todos = find_todos(&path, &text, &self.tags);
        let changed = self.files.get(&path).map_or(!todos.is_empty(), |old| *old != todos);
        if todos.is_empty() {
            self.files.remove(&path);
        } else {
            self.files.insert(path.clone(), todos.clone());
        }
        self.report(ctx, &path);
        self.highlight(ctx, &path, &todos);
        if changed {
            for view in VIEWS {
                ctx.refresh_tree(view, None);
            }
        }
    }

    fn highlight(&self, ctx: &mut Context, path: &Path, todos: &[Todo]) {
        let on = ctx.configuration("todoTree.highlight").as_bool().unwrap_or(true);
        // FIXMEs look like errors; every other tag like TODO.
        for kind in ["TODO", "FIXME"] {
            let decorations: Vec<Decoration> = todos
                .iter()
                .filter(|t| on && (kind == "FIXME") == (t.tag == "FIXME"))
                .map(|t| Decoration::new(t.range()).hover(format!("**{}** in the TODO Tree: {}", t.tag, t.text)))
                .collect();
            ctx.set_decorations(&format!("todoTree.{}", kind.to_lowercase()), path, &decorations);
        }
    }

    fn todo_item(t: &Todo) -> TreeItem {
        let label = if t.text.is_empty() { t.tag.clone() } else { t.text.clone() };
        let icon = if t.tag == "FIXME" { "$(warning)" } else { "$(circle-outline)" };
        TreeItem::new(t.id(), label)
            .description(format!("{} · line {}", t.tag, t.line + 1))
            .icon(icon)
            .context_value("todo")
            .command(CommandCall::new("todoTree.open", vec![json!(t.path), json!(t.line), json!(t.col)]))
    }

    fn find(&self, id: &str) -> Option<&Todo> {
        self.files.values().flatten().find(|t| t.id() == id)
    }

    fn all(&self) -> impl Iterator<Item = &Todo> {
        self.files.values().flatten()
    }
}

impl Extension for TodoTree {
    fn activate(ctx: &mut Context) -> Self {
        let mut ext = TodoTree { tags: Self::read_tags(ctx), files: BTreeMap::new() };
        ctx.create_decoration_type(
            "todoTree.todo",
            &DecorationOptions {
                background_color: Some(Color::hex("#ffbd2a40")),
                border_color: Some(Color::hex("#ffbd2a")),
                overview_ruler_color: Some(Color::hex("#ffbd2a")),
                ..Default::default()
            },
        );
        ctx.create_decoration_type(
            "todoTree.fixme",
            &DecorationOptions {
                background_color: Some(Color::hex("#f1484940")),
                color: Some(Color::theme("editorError.foreground")),
                overview_ruler_color: Some(Color::theme("editorError.foreground")),
                ..Default::default()
            },
        );
        ctx.register_hover_provider("todo-hover", &["*".into()]);
        ctx.register_completion_provider("todo-tags", &["*".into()], &[]);
        ctx.register_definition_provider("todo-links", &["*".into()]);
        ext.scan(ctx);
        ext
    }

    fn command(&mut self, ctx: &mut Context, command: &str, args: &[Value]) -> Result<Value, String> {
        match command {
            "todoTree.refresh" => {
                self.tags = Self::read_tags(ctx);
                self.scan(ctx);
                Ok(Value::Null)
            }
            "todoTree.open" => {
                let path = PathBuf::from(args.first().and_then(Value::as_str).ok_or("no file")?);
                let line = args.get(1).and_then(Value::as_u64).unwrap_or(0) as u32;
                let col = args.get(2).and_then(Value::as_u64).unwrap_or(0) as u32;
                ctx.show_text_document(&path, Some(Range::new(Position::new(line, col), Position::new(line, col))));
                Ok(Value::Null)
            }
            "todoTree.markDone" => {
                // From a tree item (its `id`), or the TODO under the cursor.
                let todo = match args.first().and_then(|a| a["id"].as_str()) {
                    Some(id) => self.find(id).cloned(),
                    None => {
                        let ed = ctx.active_text_editor().ok_or("no editor")?;
                        let (path, line) = (ed.document.path.clone().ok_or("untitled")?, ed.selection().start.line);
                        self.files.get(&path).and_then(|t| t.iter().find(|t| t.line == line)).cloned()
                    }
                };
                let todo = todo.ok_or("no TODO there")?;
                let mut edit = WorkspaceEdit::new();
                edit.replace(&todo.path, todo.range(), "DONE");
                if !ctx.apply_edit(&edit) {
                    return Err("couldn't edit the file".into());
                }
                if let Some(doc) = ctx.document(&todo.path) {
                    self.update(ctx, &doc);
                }
                Ok(Value::Null)
            }
            "todoTree.copy" => {
                let todo = args.first().and_then(|a| a["id"].as_str()).and_then(|id| self.find(id)).ok_or("no TODO")?;
                ctx.append_output_line("TODO Tree", &format!("{}:{}: {}: {}", todo.path.display(), todo.line + 1, todo.tag, todo.text));
                ctx.show_output("TODO Tree", true);
                Ok(Value::Null)
            }
            _ => Err(format!("command '{command}' not found")),
        }
    }

    fn tree_children(&mut self, ctx: &mut Context, view: &str, element: Option<&str>) -> Result<Vec<TreeItem>, String> {
        let folders = ctx.workspace_folders.clone();
        let relative = |p: &Path| folders.iter().find_map(|f| p.strip_prefix(f).ok()).unwrap_or(p).to_path_buf();
        Ok(match (view, element) {
            ("todoTree.todos" | "todoTree.explorer", None) => self
                .files
                .iter()
                .map(|(path, todos)| {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let dir = relative(path).parent().map(|d| d.display().to_string()).unwrap_or_default();
                    TreeItem::new(format!("file:{}", path.display()), name).description(format!("{dir}  {}", todos.len()).trim().to_string()).icon("$(file)").collapsible(Collapsible::Expanded)
                })
                .collect(),
            ("todoTree.todos" | "todoTree.explorer", Some(file)) => {
                let path = PathBuf::from(file.strip_prefix("file:").unwrap_or(file));
                self.files.get(&path).into_iter().flatten().map(Self::todo_item).collect()
            }
            ("todoTree.tags", None) => self
                .tags
                .iter()
                .map(|tag| {
                    let n = self.all().filter(|t| &t.tag == tag).count();
                    TreeItem::new(format!("tag:{tag}"), tag.clone()).description(n.to_string()).icon("$(symbol-keyword)").collapsible(if n > 0 { Collapsible::Collapsed } else { Collapsible::None })
                })
                .collect(),
            ("todoTree.tags", Some(tag)) => {
                let tag = tag.strip_prefix("tag:").unwrap_or(tag);
                self.all().filter(|t| t.tag == tag).map(|t| Self::todo_item(t).description(format!("{}:{}", relative(&t.path).display(), t.line + 1))).collect()
            }
            _ => return Err(format!("no data for view '{view}'")),
        })
    }

    fn did_open(&mut self, ctx: &mut Context, doc: &Document) {
        self.update(ctx, doc);
    }

    fn did_change(&mut self, ctx: &mut Context, doc: &Document) {
        self.update(ctx, doc);
    }

    fn configuration_changed(&mut self, ctx: &mut Context) {
        let tags = Self::read_tags(ctx);
        if tags != self.tags {
            self.tags = tags;
            self.scan(ctx);
        }
    }

    fn workspace_folders_changed(&mut self, ctx: &mut Context) {
        self.scan(ctx);
    }

    fn provide_hover(&mut self, _ctx: &mut Context, _provider: &str, doc: &Document, pos: Position) -> Option<Hover> {
        let line = doc.text.as_deref()?.lines().nth(pos.line as usize)?;
        let tag = self.tags.iter().find(|t| line.match_indices(t.as_str()).any(|(i, _)| i as u32 <= pos.character && pos.character <= (i + t.len()) as u32))?;
        let here = doc.path.as_ref().and_then(|p| self.files.get(p)).map_or(0, |t| t.iter().filter(|t| &t.tag == tag).count());
        let total = self.all().filter(|t| &t.tag == tag).count();
        Some(Hover::new(format!("**{tag}**: {here} in this file, {total} in the workspace")))
    }

    fn provide_completion(&mut self, _ctx: &mut Context, _provider: &str, doc: &Document, pos: Position, _trigger: Option<&str>) -> Vec<CompletionItem> {
        let Some(line) = doc.text.as_deref().and_then(|t| t.lines().nth(pos.line as usize)) else { return Vec::new() };
        let before = &line[..(pos.character as usize).min(line.len())];
        if !["//", "#", "<!--", "/*", "--"].iter().any(|c| before.contains(c)) {
            return Vec::new();
        }
        self.tags.iter().map(|t| CompletionItem::new(t.clone(), CompletionItemKind::Keyword).detail("TODO Tree tag").snippet(format!("{t}: ${{1:what}}"))).collect()
    }

    /// `see src/main.rs:42` in a TODO's text: Go to Definition opens it.
    fn provide_definition(&mut self, ctx: &mut Context, _provider: &str, doc: &Document, pos: Position) -> Vec<Location> {
        let Some(line) = doc.text.as_deref().and_then(|t| t.lines().nth(pos.line as usize)) else { return Vec::new() };
        if !self.tags.iter().any(|t| line.contains(t.as_str())) {
            return Vec::new();
        }
        let at = pos.character as usize;
        let start = line[..at.min(line.len())].rfind(|c: char| c.is_whitespace() || c == '(').map_or(0, |i| i + 1);
        let end = line[at.min(line.len())..].find(|c: char| c.is_whitespace() || c == ')').map_or(line.len(), |i| at + i);
        let word = line[start..end].trim_end_matches(['.', ',']);
        let Some((file, n)) = word.rsplit_once(':') else { return Vec::new() };
        let Ok(n) = n.parse::<u32>() else { return Vec::new() };
        let bases: Vec<PathBuf> = doc.path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).into_iter().chain(ctx.workspace_folders.clone()).collect();
        bases
            .iter()
            .map(|b| b.join(file))
            .find(|p| p.is_file())
            .map(|p| vec![Location::new(p, Range::new(Position::new(n.saturating_sub(1), 0), Position::new(n.saturating_sub(1), 0)))])
            .unwrap_or_default()
    }
}

fn main() {
    orbvane_extension::run::<TodoTree>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tags() {
        let tags = vec!["TODO".to_string(), "FIXME".to_string()];
        let text = "fn a() {} // TODO: write b\n// FIXME(sam) it breaks */\nlet TODOS = 1; // NOTTODO\n# TODO";
        let found = find_todos(Path::new("/w/a.rs"), text, &tags);
        let short: Vec<(u32, u32, &str, &str)> = found.iter().map(|t| (t.line, t.col, t.tag.as_str(), t.text.as_str())).collect();
        assert_eq!(short, [(0, 13, "TODO", "write b"), (1, 3, "FIXME", "it breaks"), (3, 2, "TODO", "")]);
    }
}
