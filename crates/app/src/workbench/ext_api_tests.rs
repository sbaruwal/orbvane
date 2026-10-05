//! The TODO Tree example (`examples/extensions/todo-tree`, built by `cargo build`) against the
//! editor: its tree views, decorations, diagnostics, hover, completion and definition providers,
//! and an inline tree item action.

use std::path::Path;
use std::time::{Duration, Instant};

use text::{Pos, Selection};

use super::ext_views::TreeHit;
use super::{View, Workbench};

fn wait_for(wb: &mut Workbench, what: &str, done: impl Fn(&mut Workbench) -> bool) {
    let start = Instant::now();
    while !done(wb) {
        assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {what}: toasts {:?}, log {:?}", wb.toast_list(), wb.output.lines("TODO Tree"));
        std::thread::sleep(Duration::from_millis(10));
        wb.ext_tick();
        wb.ext_views_sync();
        wb.ext_decorations_tick();
        wb.lsp_tick();
    }
}

fn labels(wb: &mut Workbench, vi: usize) -> Vec<String> {
    wb.ext_tree_rows(vi).0.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.item.label)).collect()
}

#[test]
fn runs_the_todo_tree_example() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    if !root.join("target/debug/todo-tree").exists() {
        eprintln!("skipped: build the example first (cargo build -p todo-tree)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("orbvane-todo-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let ws = dir.join("ws").canonicalize().unwrap();
    let notes = ws.join("notes.txt");
    std::fs::write(&notes, "TODO: first\n# FIXME: broken, see notes.txt:1\n\n").unwrap();
    // SAFETY: every test that reads this wants the same scratch user data folder.
    unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
    let mut wb = Workbench::new(Some(ws.clone()), &[], std::sync::Arc::new(|| {}));
    let id = crate::contributions::with_mut(|r| {
        *r = extensions::Registry::scan(&dir.join("extensions"));
        r.install_folder(&root.join("examples/extensions/todo-tree"))
    })
    .unwrap();
    assert_eq!(id, "orbvane.todo-tree");
    crate::contributions::register(&crate::contributions::enabled()[0]);
    wb.ext_views_sync();

    // Its container is in the activity bar, with two views; a third is in the Explorer.
    let containers = wb.ext_view_containers();
    assert_eq!(containers.len(), 1);
    let View::Ext(c) = containers[0] else { panic!() };
    assert_eq!(crate::contributions::container(c).unwrap().title, "TODO Tree");
    assert_eq!(wb.ext_views_in("todoTree").len(), 2);
    let explorer = wb.explorer_ext_views();
    assert_eq!(explorer.len(), 1);
    let todos = wb.ext_views_in("todoTree")[0];

    // The tree: the file, then its TODOs (asking starts the extension).
    wait_for(&mut wb, "the tree", |wb| labels(wb, todos).len() == 3);
    assert_eq!(labels(&mut wb, todos), ["notes.txt", "  first", "  broken, see notes.txt:1"]);
    let in_explorer = explorer[0];
    wait_for(&mut wb, "the Explorer's view", |wb| labels(wb, in_explorer).len() == 3);

    // Opening the file: highlights, and the FIXME as a warning.
    wb.open_file(&notes);
    let doc = wb.active_editor().unwrap().doc;
    wait_for(&mut wb, "the decorations", |wb| wb.ext_decorations_for(doc).len() == 2);
    let decos = wb.ext_decorations_for(doc);
    assert_eq!((decos[0].start, decos[0].end), (Pos::new(0, 0), Pos::new(0, 4)));
    assert!(decos.iter().any(|d| d.start == Pos::new(1, 2) && d.ruler.is_some()));
    wait_for(&mut wb, "the problem", |wb| wb.lsp.diagnostics.get(&notes).is_some_and(|(_, d)| d.len() == 1));
    let diag = wb.lsp.diagnostics[&notes].1[0].clone();
    assert_eq!((diag.severity, diag.message.as_str(), diag.range.start.line), (lsp::Severity::Warning, "FIXME: broken, see notes.txt:1", 1));

    // Decorations move with edits.
    if let Some((ed, d)) = wb.active_mut() {
        ed.set_selection(Selection::caret(Pos::new(0, 0)));
        ed.type_text(d, "\n");
    }
    wait_for(&mut wb, "the moved decorations", |wb| wb.ext_decorations_for(doc).iter().any(|d| d.start == Pos::new(1, 0)));

    // Hover over a tag: the decoration's message and the provider's.
    wb.hover_probe = Some(super::intel::HoverProbe { group: wb.active_group, doc, pos: Pos::new(1, 1), since: Instant::now() - Duration::from_secs(5), fired: false });
    wb.fire_hover_probe();
    wait_for(&mut wb, "the hover", |wb| wb.hover.as_ref().and_then(|h| h.markdown.as_ref()).is_some_and(|m| m.contains("in the workspace")));
    let hover = wb.hover.as_ref().unwrap().markdown.clone().unwrap();
    assert!(hover.contains("**TODO** in the TODO Tree: first") && hover.contains("**TODO**: 1 in this file, 1 in the workspace"), "{hover}");
    wb.hover = None;

    // Completion in a comment offers the tags.
    if let Some((ed, d)) = wb.active_mut() {
        ed.set_selection(Selection::caret(Pos::new(3, 0)));
        ed.type_text(d, "# FI");
    }
    wb.trigger_completion(None, false);
    wait_for(&mut wb, "the suggestions", |wb| wb.completion.as_ref().is_some_and(|c| c.items.iter().any(|i| i.label == "FIXME")));
    wb.completion = None;

    // Go to Definition on "notes.txt:1" (no language server for text files).
    let line2 = wb.active_doc().unwrap().buffer.line(2);
    let col = line2.find("notes.txt").unwrap() + 3;
    wb.go_to_definition(Some(Pos::new(2, col)));
    wait_for(&mut wb, "the definition", |wb| wb.active_editor().is_some_and(|e| e.sel.head == Pos::new(0, 0)));

    // The inline action on a TODO: Mark as Done edits the file.
    wait_for(&mut wb, "the tree", |wb| labels(wb, todos).len() == 3);
    let row = wb.ext_tree_rows(todos).0.iter().position(|r| r.item.label == "first").unwrap();
    wb.ext_tree_click(todos, TreeHit::Inline(row as u32, 0), 0.0, 0.0);
    wait_for(&mut wb, "the edit", |wb| wb.active_doc().unwrap().buffer.line(1).starts_with("DONE: first"));
    wait_for(&mut wb, "the tree to follow", |wb| labels(wb, todos) == ["notes.txt", "  broken, see notes.txt:1"]);

    // Stopping it removes what it added.
    wb.ext_stop(&id);
    assert!(wb.ext_decorations_for(doc).is_empty());
    assert!(!wb.lsp.diagnostics.contains_key(&notes));
    crate::contributions::with_mut(|r| *r = extensions::Registry::default());
    let _ = std::fs::remove_dir_all(&dir);
}
