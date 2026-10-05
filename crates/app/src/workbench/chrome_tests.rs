//! The window chrome, drawn for real (offscreen): the toolbar, the sidebar's view switcher and
//! the secondary side bar.

use super::*;

fn draw(wb: &mut Workbench, r: &mut render::Renderer) {
    let bg = wb.background();
    r.frame(bg, |c| wb.draw(c));
}

/// The center of `hit`'s rectangle in the last frame.
fn spot(wb: &Workbench, hit: Hit) -> (f32, f32) {
    let (r, _) = wb.hits.iter().rev().find(|(_, h)| *h == hit).unwrap_or_else(|| panic!("{hit:?} isn't drawn"));
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn click(wb: &mut Workbench, r: &mut render::Renderer, hit: Hit) {
    let (x, y) = spot(wb, hit);
    wb.mouse_down(x, y, false, false, false);
    wb.mouse_up();
    // Far enough apart that the next click isn't a double click.
    wb.last_click = None;
    draw(wb, r);
}

#[test]
fn toolbar_and_switcher() {
    let dir = std::env::temp_dir().join(format!("orbvane-chrome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    // SAFETY: every test that reads this wants the same scratch user data folder.
    unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
    let mut wb = Workbench::new(Some(dir.clone()), &[dir.join("notes.txt")], std::sync::Arc::new(|| {}));
    let Ok(mut r) = render::Renderer::offscreen((1100, 700), 1.0) else { return }; // no GPU here
    draw(&mut wb, &mut r);
    // Something was drawn.
    let (w, h, px) = r.pixels();
    assert_eq!(px.len(), (w * h * 4) as usize);
    assert!(px.chunks_exact(4).any(|p| p != &px[..4]));

    // No activity bar: the sidebar starts at the window's edge, the switcher at its top.
    let (sx, sy) = spot(&wb, Hit::Activity(View::Explorer));
    assert!(sx < 40.0 && sy > TITLE_H && sy < TITLE_H + SWITCHER_H, "{sx},{sy}");
    assert!(spot(&wb, Hit::Manage).1 < TITLE_H);

    // Switching views, and the active view's button hides the sidebar.
    click(&mut wb, &mut r, Hit::Activity(View::Search));
    assert!(wb.view == View::Search && wb.sidebar_visible);
    click(&mut wb, &mut r, Hit::Activity(View::Search));
    assert!(!wb.sidebar_visible);
    // The toolbar's sidebar toggle brings it back.
    click(&mut wb, &mut r, Hit::ToggleSidebarButton);
    assert!(wb.sidebar_visible);

    // The search field opens Go to File; the project pill, Open Recent.
    click(&mut wb, &mut r, Hit::CommandCenter);
    assert!(wb.palette.as_ref().is_some_and(|p| p.picker.is_none()));
    wb.cancel_palette();
    draw(&mut wb, &mut r);
    click(&mut wb, &mut r, Hit::ToolbarProject);
    assert!(wb.palette.is_some());
    wb.cancel_palette();

    // A narrow sidebar puts the views that don't fit behind "...".
    wb.sidebar_w = 120.0;
    wb.mouse_move(600.0, 400.0);
    draw(&mut wb, &mut r);
    assert!(wb.hits.iter().any(|(_, h)| *h == Hit::SwitcherMore));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn secondary_side_bar() {
    let dir = std::env::temp_dir().join(format!("orbvane-aux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    // SAFETY: every test that reads this wants the same scratch user data folder.
    unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
    let mut wb = Workbench::new(Some(dir.clone()), &[dir.join("notes.txt")], std::sync::Arc::new(|| {}));
    let Ok(mut r) = render::Renderer::offscreen((1100, 700), 1.0) else { return }; // no GPU here
    draw(&mut wb, &mut r);
    assert!(!wb.aux.visible && !wb.hits.iter().any(|(_, h)| *h == Hit::AuxBody));

    // The toolbar button shows it on the right, with the Assistant's setup (no agent yet).
    click(&mut wb, &mut r, Hit::ToggleAuxButton);
    assert!(wb.aux.visible);
    let (bx, _) = spot(&wb, Hit::AuxBody);
    assert!(bx > 1100.0 - wb.aux.width, "{bx}");
    assert!(wb.hits.iter().any(|(_, h)| *h == Hit::AssistantAgent(0)));
    // The editors end where it starts.
    let editor = wb.hits.iter().find(|(_, h)| matches!(h, Hit::Editor(_))).map(|(r, _)| *r).unwrap();
    assert!(editor.right() <= 1100.0 - wb.aux.width + 1.0, "{editor:?}");

    // Its tabs: the Outline.
    click(&mut wb, &mut r, Hit::AuxTab(1));
    assert_eq!(wb.aux.tab, aux_bar::AuxTab::Outline);
    assert!(wb.hits.iter().any(|(_, h)| *h == Hit::OutlineBody));

    // Dragging its sash makes it wider.
    let (sx, sy) = spot(&wb, Hit::AuxSash);
    wb.mouse_down(sx, sy, false, false, false);
    wb.mouse_move(sx - 60.0, sy);
    wb.mouse_up();
    assert!((wb.aux.width - 420.0).abs() < 1.0, "{}", wb.aux.width);
    draw(&mut wb, &mut r);

    // The close button, and ⌥⌘B.
    click(&mut wb, &mut r, Hit::AuxClose);
    assert!(!wb.aux.visible);
    wb.run(crate::commands::Command::ToggleAuxiliaryBar);
    assert!(wb.aux.visible);
    // Ask the Agent goes to the Assistant's message box.
    wb.run(crate::commands::Command::AssistantFocus);
    assert!(wb.aux.tab == aux_bar::AuxTab::Assistant && wb.focus == Focus::Assistant);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A list's rows never take clicks outside it: the last, partly shown row of the folder tree
/// used to reach under the header below it, so clicking that header opened a file.
#[test]
fn rows_stay_inside_their_lists() {
    let dir = std::env::temp_dir().join(format!("orbvane-rows-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..80 {
        std::fs::write(dir.join(format!("file{i:02}.txt")), "x\n").unwrap();
    }
    // SAFETY: every test that reads this wants the same scratch user data folder.
    unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
    let mut wb = Workbench::new(Some(dir.clone()), &[dir.join("file00.txt")], std::sync::Arc::new(|| {}));
    let Ok(mut r) = render::Renderer::offscreen((1000, 613), 1.0) else { return }; // no GPU here
    // Scrolled so a row is cut off at the bottom.
    wb.tree.as_mut().unwrap().scroll = 7.0;
    draw(&mut wb, &mut r);
    let body = wb.sections.body[0];
    let rows: Vec<Rect> = wb.hits.iter().filter(|(_, h)| matches!(h, Hit::ExplorerRow(_))).map(|(r, _)| *r).collect();
    assert!(rows.len() > 10);
    for row in rows {
        assert!(row.y >= body.y - 0.01 && row.bottom() <= body.bottom() + 0.01, "{row:?} outside {body:?}");
    }
    // The Explorer has no title row: the project's name heads the tree, under the switcher.
    let head = wb.sections.head[0];
    assert!((head.y - (TITLE_H + SWITCHER_H)).abs() < 0.5, "{head:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Help → Welcome: the page's links, themes and recent folders are drawn and clickable, and it
/// takes no typing.
#[test]
fn welcome_page() {
    let dir = std::env::temp_dir().join(format!("orbvane-welcome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: every test that reads this wants the same scratch user data folder.
    unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
    let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
    let Ok(mut r) = render::Renderer::offscreen((1200, 800), 1.0) else { return };
    wb.run(Command::Welcome);
    assert!(wb.active_editor().is_some_and(|e| e.welcome));
    draw(&mut wb, &mut r);
    use welcome::WelcomeHit;
    for hit in [WelcomeHit::Run(Command::NewFile), WelcomeHit::Run(Command::OpenFolder), WelcomeHit::Run(Command::GitClone), WelcomeHit::Theme(0), WelcomeHit::Theme(2), WelcomeHit::ShowOnStartup] {
        spot(&wb, Hit::Welcome(hit));
    }
    if let Some(out) = std::env::var_os("ORBVANE_WELCOME_SNAPSHOT") {
        let (w, h, px) = r.pixels();
        render::write_png(std::path::Path::new(&out), w, h, &px).unwrap();
    }
    // Typing goes nowhere; a link runs its command.
    wb.key(crate::input::KeyInput { key: crate::input::Key::Char("x".into()), text: Some("x".into()), cmd: false, shift: false, alt: false, ctrl: false });
    assert!(wb.docs[wb.active_editor().unwrap().doc].as_ref().unwrap().buffer.text().is_empty());
    click(&mut wb, &mut r, Hit::Welcome(WelcomeHit::Run(Command::CommandPalette)));
    assert!(wb.palette.is_some());
    // Opened again, it's the same tab.
    wb.palette = None;
    wb.run(Command::Welcome);
    assert_eq!(wb.groups.iter().map(|g| g.tabs.iter().filter(|t| t.welcome).count()).sum::<usize>(), 1);
    wb.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
