//! orbvane: a editor written in Rust.
//!
//! This file is the platform layer: the native macOS window and menu bar, and translation of
//! winit events into workbench input.

mod agents;
mod brackets;
mod colors;
mod commands;
mod contributions;
mod config;
mod conflicts;
mod cursors;
mod diff_view;
mod editor;
mod emmet;
mod explorer;
mod folding;
mod icons;
mod imageio;
mod input;
mod json_schemas;
mod keymap;
mod languages;
mod layout;
mod markdown;
mod merge;
mod search_editor;
mod servers;
mod runnables;
mod snippet;
mod testing;
mod trash;
mod updater;
mod when;
mod widgets;
mod workspace;
mod palette;
mod workbench;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use muda::{CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey};
use winit::platform::macos::WindowAttributesExtMacOS;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Window, WindowId};

use commands::Command;
use input::{Key, KeyInput};
use workbench::{CursorKind, Effect, PopupItem, Workbench};

enum UserEvent {
    Menu(muda::MenuId),
    /// A language server or terminal produced output; redraw to pick it up.
    Wake,
    /// The workbench agreed to close the window.
    Exit,
}

/// The app's state. It lives outside winit's handler (see `Shared`) so that input can be
/// handled from the main dispatch queue.
struct Core {
    window: Option<Arc<Window>>,
    renderer: Option<render::Renderer>,
    workbench: Workbench,
    modifiers: ModifiersState,
    proxy: EventLoopProxy<UserEvent>,
    /// Last command run from a keybinding, to drop the duplicate the menu may also send.
    last_key_command: Option<(Command, Instant)>,
    /// The session was saved and the app is ending.
    quitting: bool,
}

/// Input waiting to be handled.
enum Input {
    Window(WindowEvent),
    Menu(muda::MenuId),
}

impl Input {
    /// Clicks, keys and menu picks, which a modal dialog blocks. (Releases still go through,
    /// so a drag that started before the dialog ends.)
    fn is_user_input(&self) -> bool {
        match self {
            Input::Menu(_) => true,
            Input::Window(e) => matches!(
                e,
                WindowEvent::MouseInput { state: ElementState::Pressed, .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::DroppedFile(_)
                    | WindowEvent::CloseRequested
            ),
        }
    }
}

/// Input is handled outside winit's event handler, from a block on the main dispatch queue.
///
/// Native dialogs (`rfd`'s `show()`, file pickers) run a nested run loop until they close. If
/// that happened inside winit's handler, an event AppKit delivers meanwhile (the second click
/// of a double-click on "Commit") would re-enter winit's handler, and winit aborts. Outside
/// the handler, winit takes such events normally; they reach `App::push` while `core` is
/// borrowed by the dialog's caller and are dropped, like macOS does for a window behind a
/// modal dialog.
struct Shared {
    core: RefCell<Core>,
    queue: RefCell<VecDeque<Input>>,
    /// A drain block is on the main queue.
    scheduled: Cell<bool>,
}

thread_local! {
    static SHARED: RefCell<Option<Rc<Shared>>> = const { RefCell::new(None) };
}

fn shared() -> Rc<Shared> {
    SHARED.with(|s| s.borrow().clone()).expect("app state not set")
}

/// Handles queued input. Runs from the main dispatch queue, outside winit's handler.
fn drain() {
    let shared = shared();
    shared.scheduled.set(false);
    // Borrowed: we're inside a dialog opened while handling earlier input. That outer loop
    // continues with the queue once the dialog closes.
    let Ok(mut core) = shared.core.try_borrow_mut() else { return };
    let mut handled = false;
    loop {
        let Some(input) = shared.queue.borrow_mut().pop_front() else { break };
        core.handle(input);
        handled = true;
    }
    if handled {
        core.apply_effects();
        core.redraw();
    }
}

struct App {
    shared: Rc<Shared>,
    menu: Option<Menu>,
}

impl App {
    /// Queues input for `drain`.
    fn push(&self, input: Input) {
        let busy = self.shared.core.try_borrow_mut().is_err();
        if busy && input.is_user_input() {
            return; // a dialog is open
        }
        self.shared.queue.borrow_mut().push_back(input);
        if !self.shared.scheduled.replace(true) {
            dispatch2::DispatchQueue::main().exec_async(drain);
        }
    }
}

thread_local! {
    /// Menu items with shortcuts, to update when the keymap changes.
    static MENU_ITEMS: std::cell::RefCell<Vec<(Command, MenuItem)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn menu_item(cmd: Command) -> MenuItem {
    let accel = keymap::accelerator(cmd).and_then(|a| a.parse().ok());
    let item = MenuItem::with_id(cmd.id(), cmd.menu_label(), true, accel);
    MENU_ITEMS.with(|m| m.borrow_mut().push((cmd, item.clone())));
    item
}

thread_local! {
    /// File > Open Recent, rebuilt when the recent folders change.
    static RECENT_MENU: std::cell::RefCell<Option<Submenu>> = const { std::cell::RefCell::new(None) };
}

/// Fills File > Open Recent: the folders (ids `recent:<index>`), More..., and Clear.
fn update_recent_menu(folders: &[(String, PathBuf)]) {
    RECENT_MENU.with(|m| {
        let Some(menu) = &*m.borrow() else { return };
        while menu.remove_at(0).is_some() {}
        for (i, (label, _)) in folders.iter().enumerate() {
            let _ = menu.append(&MenuItem::with_id(format!("recent:{i}"), label, true, None));
        }
        if !folders.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let _ = menu.append(&menu_item(Command::OpenRecent));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&menu_item(Command::ClearRecent));
    });
}

/// Shows the keymap's shortcuts in the menu bar (and lets them trigger the items).
fn update_menu_accelerators() {
    MENU_ITEMS.with(|m| {
        for (cmd, item) in m.borrow().iter() {
            let _ = item.set_accelerator(keymap::accelerator(*cmd).and_then(|a| a.parse().ok()));
        }
    });
}

fn build_menu() -> Menu {
    let sep = PredefinedMenuItem::separator;
    let recent = Submenu::new("Open Recent", true);
    RECENT_MENU.with(|m| *m.borrow_mut() = Some(recent.clone()));
    let about = muda::AboutMetadata { name: Some("Orbvane".into()), version: Some(env!("CARGO_PKG_VERSION").into()), ..Default::default() };
    // Like Code > Settings submenu.
    let themes = Submenu::with_items("Themes", true, &[&menu_item(Command::SelectTheme)]).unwrap();
    let settings = Submenu::with_items(
        "Settings",
        true,
        &[&menu_item(Command::OpenSettings), &menu_item(Command::OpenKeybindings), &sep(), &themes],
    )
    .unwrap();
    let app = Submenu::with_items(
        "Orbvane",
        true,
        &[
            &PredefinedMenuItem::about(Some("About Orbvane"), Some(about)),
            &menu_item(Command::CheckForUpdates),
            &sep(),
            &settings,
            &sep(),
            &PredefinedMenuItem::services(None),
            &sep(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &sep(),
            // Our own Quit (not the predefined one) so unsaved changes are kept or asked about.
            &menu_item(Command::Quit),
        ],
    )
    .unwrap();
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &menu_item(Command::NewFile),
            &sep(),
            &menu_item(Command::OpenFolder),
            &menu_item(Command::OpenWorkspaceFromFile),
            &recent,
            &sep(),
            &menu_item(Command::AddFolderToWorkspace),
            &menu_item(Command::SaveWorkspaceAs),
            &sep(),
            &menu_item(Command::Save),
            &menu_item(Command::SaveAs),
            &menu_item(Command::SaveAll),
            &sep(),
            &menu_item(Command::RevertFile),
            &menu_item(Command::CloseEditor),
            &menu_item(Command::CloseFolder),
        ],
    )
    .unwrap();
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &menu_item(Command::Undo),
            &menu_item(Command::Redo),
            &sep(),
            &menu_item(Command::Cut),
            &menu_item(Command::Copy),
            &menu_item(Command::Paste),
            &sep(),
            &menu_item(Command::Find),
            &menu_item(Command::FindReplace),
            &menu_item(Command::FindNext),
            &menu_item(Command::FindPrevious),
            &sep(),
            &menu_item(Command::ShowSearch),
            &menu_item(Command::ReplaceInFiles),
            &sep(),
            &menu_item(Command::ToggleComment),
            &menu_item(Command::EmmetExpandAbbreviation),
        ],
    )
    .unwrap();
    let selection = Submenu::with_items(
        "Selection",
        true,
        &[
            &menu_item(Command::SelectAll),
            &sep(),
            &menu_item(Command::InsertCursorAbove),
            &menu_item(Command::InsertCursorBelow),
            &menu_item(Command::InsertCursorAtLineEnds),
            &menu_item(Command::AddNextOccurrence),
            &menu_item(Command::AddPreviousOccurrence),
            &menu_item(Command::SelectAllOccurrences),
        ],
    )
    .unwrap();
    let appearance = Submenu::with_items(
        "Appearance",
        true,
        &[&menu_item(Command::ToggleZenMode), &sep(), &menu_item(Command::ToggleSidebar), &menu_item(Command::TogglePanel), &menu_item(Command::ToggleAuxiliaryBar), &menu_item(Command::ToggleMinimap)],
    )
    .unwrap();
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &menu_item(Command::CommandPalette),
            &sep(),
            &appearance,
            &menu_item(Command::SplitEditor),
            &sep(),
            &menu_item(Command::ToggleTerminal),
            &menu_item(Command::ToggleOutput),
            &menu_item(Command::ToggleDebugConsole),
            &sep(),
            &menu_item(Command::ShowExplorer),
            &menu_item(Command::ShowSearch),
            &menu_item(Command::ShowScm),
            &menu_item(Command::ShowDebug),
            &menu_item(Command::ShowExtensions),
            &menu_item(Command::ShowTesting),
            &sep(),
            &menu_item(Command::ToggleWordWrap),
        ],
    )
    .unwrap();
    let go = Submenu::with_items(
        "Go",
        true,
        &[
            &menu_item(Command::QuickOpen),
            &sep(),
            &menu_item(Command::ShowAllSymbols),
            &sep(),
            &menu_item(Command::GotoSymbol),
            &menu_item(Command::GoToDefinition),
            &menu_item(Command::GoToReferences),
            &sep(),
            &menu_item(Command::GotoLine),
            &menu_item(Command::JumpToBracket),
            &sep(),
            &menu_item(Command::FocusGroup1),
            &menu_item(Command::FocusGroup2),
            &menu_item(Command::FocusGroup3),
        ],
    )
    .unwrap();
    let new_breakpoint =
        Submenu::with_items("New Breakpoint", true, &[&menu_item(Command::ConditionalBreakpoint), &menu_item(Command::AddLogpoint)]).unwrap();
    let run = Submenu::with_items(
        "Run",
        true,
        &[
            &menu_item(Command::DebugStart),
            &menu_item(Command::DebugRun),
            &menu_item(Command::DebugStop),
            &menu_item(Command::DebugRestart),
            &sep(),
            &menu_item(Command::DebugConfigure),
            &sep(),
            &menu_item(Command::DebugStepOver),
            &menu_item(Command::DebugStepInto),
            &menu_item(Command::DebugStepOut),
            &menu_item(Command::DebugContinue),
            &sep(),
            &menu_item(Command::ToggleBreakpoint),
            &new_breakpoint,
            &sep(),
            &menu_item(Command::EnableAllBreakpoints),
            &menu_item(Command::DisableAllBreakpoints),
            &menu_item(Command::RemoveAllBreakpoints),
        ],
    )
    .unwrap();
    let terminal = Submenu::with_items(
        "Terminal",
        true,
        &[
            &menu_item(Command::NewTerminal),
            &menu_item(Command::SplitTerminal),
            &menu_item(Command::KillTerminal),
            &sep(),
            &menu_item(Command::RunTask),
            &menu_item(Command::RunBuildTask),
            &menu_item(Command::TerminateTask),
            &sep(),
            &menu_item(Command::ConfigureTasks),
        ],
    )
    .unwrap();
    let window = Submenu::with_items(
        "Window",
        true,
        &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(Some("Zoom")), &sep(), &PredefinedMenuItem::fullscreen(None)],
    )
    .unwrap();
    let help = Submenu::with_items("Help", true, &[&menu_item(Command::Welcome), &menu_item(Command::CommandPalette)]).unwrap();
    let menu = Menu::with_items(&[&app, &file, &edit, &selection, &view, &go, &run, &terminal, &window, &help]).unwrap();
    window.set_as_windows_menu_for_nsapp();
    menu
}

/// A point for AppKit calls (`NSPoint`).
#[repr(C)]
struct NsPoint {
    x: f64,
    y: f64,
}

// SAFETY: matches the layout and encoding of CGPoint/NSPoint.
unsafe impl objc2::Encode for NsPoint {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

/// Native items for popup entries. Every entry except a submenu gets the next "popup:<i>" id,
/// in depth-first order (matching the workbench's list of actions).
fn popup_items(items: &[PopupItem], next: &mut usize) -> Vec<Box<dyn muda::IsMenuItem>> {
    items
        .iter()
        .map(|item| -> Box<dyn muda::IsMenuItem> {
            if let PopupItem::Submenu { label, items } = item {
                let sub = Submenu::new(label, true);
                for child in popup_items(items, next) {
                    let _ = sub.append(child.as_ref());
                }
                return Box::new(sub);
            }
            let id = format!("popup:{next}");
            *next += 1;
            match item {
                PopupItem::Item { label, enabled, checked: Some(checked) } => {
                    Box::new(CheckMenuItem::with_id(id, label, *enabled, *checked, None))
                }
                PopupItem::Item { label, enabled, checked: None } => Box::new(MenuItem::with_id(id, label, *enabled, None)),
                _ => Box::new(PredefinedMenuItem::separator()),
            }
        })
        .collect()
}

/// Shows a native popup menu at a window position (logical points); a picked entry arrives as
/// a menu event with id "popup:<index>". Menu tracking runs a nested run loop, which must not
/// happen inside winit's event handler (winit panics on re-entry), so it's deferred to the
/// main queue and runs once the current event is done.
fn show_popup(window: &Window, items: Vec<PopupItem>, x: f32, y: f32) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view = h.ns_view.as_ptr() as usize;
    dispatch2::DispatchQueue::main().exec_async(move || {
        use objc2::runtime::{AnyObject, Bool};
        let menu = Menu::new();
        let mut next = 0;
        for item in popup_items(&items, &mut next) {
            let _ = menu.append(item.as_ref());
        }
        // Positioned in the view's own coordinates: winit's view is flipped (y down), like ours.
        // (muda's helper assumes an unflipped view, which puts the menu in the wrong place.)
        let ns_menu = menu.ns_menu() as *mut AnyObject;
        let view = view as *mut AnyObject;
        let at = NsPoint { x: x as f64, y: y as f64 };
        // SAFETY: both objects are live AppKit objects owned by this app, used on the main thread.
        let _: Bool = unsafe {
            objc2::msg_send![ns_menu, popUpMenuPositioningItem: std::ptr::null_mut::<AnyObject>(), atLocation: at, inView: view]
        };
    });
}

impl Core {
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn apply_effects(&mut self) {
        let Some(window) = self.window.clone() else { return };
        for effect in self.workbench.take_effects() {
            match effect {
                Effect::DragWindow => {
                    let _ = window.drag_window();
                }
                Effect::ToggleMaximize => window.set_maximized(!window.is_maximized()),
                Effect::SetFullscreen(on) => window.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None))),
                Effect::SetTitle(t) => window.set_title(&t),
                Effect::Cursor(kind) => window.set_cursor(match kind {
                    CursorKind::Default => CursorIcon::Default,
                    CursorKind::Text => CursorIcon::Text,
                    CursorKind::ColResize => CursorIcon::ColResize,
                    CursorKind::RowResize => CursorIcon::RowResize,
                    CursorKind::Pointer => CursorIcon::Pointer,
                }),
                Effect::SetAppearance { dark } => {
                    window.set_theme(Some(if dark { winit::window::Theme::Dark } else { winit::window::Theme::Light }));
                }
                Effect::Popup { items, x, y } => show_popup(&window, items, x, y),
                Effect::KeymapChanged => update_menu_accelerators(),
                Effect::RecentChanged => update_recent_menu(&self.workbench.recent_menu()),
                Effect::Exit => {
                    self.quitting = true;
                    let _ = self.proxy.send_event(UserEvent::Exit);
                }
            }
        }
    }

    fn run_command(&mut self, cmd: Command, from_menu: bool) {
        if from_menu {
            if let Some((last, t)) = self.last_key_command {
                if last == cmd && t.elapsed().as_millis() < 150 {
                    return;
                }
            }
        }
        self.workbench.run(cmd);
    }

    fn scale(&self) -> f32 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32)
    }
}

fn translate_key(event: &winit::event::KeyEvent, mods: ModifiersState) -> KeyInput {
    let key = match event.key_without_modifiers() {
        WKey::Named(named) => match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::Escape => Key::Escape,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Space,
            NamedKey::F1 => Key::F(1),
            NamedKey::F2 => Key::F(2),
            NamedKey::F3 => Key::F(3),
            NamedKey::F4 => Key::F(4),
            NamedKey::F5 => Key::F(5),
            NamedKey::F6 => Key::F(6),
            NamedKey::F7 => Key::F(7),
            NamedKey::F8 => Key::F(8),
            NamedKey::F9 => Key::F(9),
            NamedKey::F10 => Key::F(10),
            NamedKey::F11 => Key::F(11),
            NamedKey::F12 => Key::F(12),
            _ => Key::Other,
        },
        WKey::Character(s) => Key::Char(s.to_lowercase()),
        _ => Key::Other,
    };
    let text = event.text.as_ref().map(|t| t.to_string()).filter(|t| !t.chars().any(char::is_control));
    KeyInput {
        key,
        text,
        cmd: mods.super_key(),
        shift: mods.shift_key(),
        alt: mods.alt_key(),
        ctrl: mods.control_key(),
    }
}

impl Core {
    /// Handles one input event (from `drain`, outside winit's handler).
    fn handle(&mut self, input: Input) {
        let event = match input {
            Input::Menu(id) => {
                if let Some(i) = id.as_ref().strip_prefix("popup:").and_then(|i| i.parse().ok()) {
                    self.workbench.popup_selected(i);
                } else if let Some(i) = id.as_ref().strip_prefix("recent:").and_then(|i| i.parse().ok()) {
                    self.workbench.open_recent(i);
                } else if let Some(cmd) = Command::from_id(id.as_ref()) {
                    self.run_command(cmd, true);
                }
                return;
            }
            Input::Window(e) => e,
        };
        let scale = self.scale();
        match event {
            // Closing the window quits (hot exit or a save prompt).
            WindowEvent::CloseRequested => self.workbench.run(Command::Quit),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height, scale);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let (Some(r), Some(w)) = (&mut self.renderer, &self.window) {
                    let size = w.inner_size();
                    r.resize(size.width, size.height, scale_factor as f32);
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                self.workbench.set_modifiers(self.modifiers.control_key(), self.modifiers.alt_key());
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.workbench.mouse_move(position.x as f32 / scale, position.y as f32 / scale);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    let (x, y) = self.workbench.mouse_position();
                    self.workbench.mouse_down(x, y, self.modifiers.shift_key(), self.modifiers.super_key(), self.modifiers.alt_key());
                }
                ElementState::Released => self.workbench.mouse_up(),
            },
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                let (x, y) = self.workbench.mouse_position();
                self.workbench.context_menu(x, y);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / scale, p.y as f32 / scale),
                    MouseScrollDelta::LineDelta(x, y) => (x * 3.0 * editor::line_height(), y * 3.0 * editor::line_height()),
                };
                self.workbench.scroll(dx, dy);
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if event.state == ElementState::Pressed {
                    let input = translate_key(&event, self.modifiers);
                    if let Some(cmd) = input.command() {
                        self.last_key_command = Some((cmd, Instant::now()));
                    }
                    self.workbench.key(input);
                }
            }
            WindowEvent::DroppedFile(path) => self.workbench.drop_path(&path),
            WindowEvent::Focused(true) => self.workbench.window_focused(),
            WindowEvent::Focused(false) => self.workbench.window_blurred(),
            _ => {}
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let mut core = self.shared.core.borrow_mut();
        if core.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title("Orbvane")
            .with_inner_size(LogicalSize::new(1400.0, 900.0))
            .with_min_inner_size(LogicalSize::new(640.0, 400.0))
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true);
        // Last session's size and position, if that spot is still on a screen.
        if let Some(b) = workbench::window_bounds() {
            let on_screen = event_loop.available_monitors().any(|m| {
                let (pos, size) = (m.position().to_logical::<f64>(m.scale_factor()), m.size().to_logical::<f64>(m.scale_factor()));
                b.x + 100.0 > pos.x && b.x < pos.x + size.width - 100.0 && b.y >= pos.y - 50.0 && b.y < pos.y + size.height - 100.0
            });
            attrs = attrs.with_inner_size(LogicalSize::new(b.width.max(640.0), b.height.max(400.0))).with_maximized(b.maximized);
            if on_screen {
                attrs = attrs.with_position(winit::dpi::LogicalPosition::new(b.x, b.y));
            }
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("failed to create window"));
        let size = window.inner_size();
        let renderer = render::Renderer::new(window.clone(), (size.width, size.height), window.scale_factor() as f32)
            .expect("failed to initialize GPU renderer");

        let menu = build_menu();
        menu.init_for_nsapp();
        update_recent_menu(&core.workbench.recent_menu());
        let proxy = core.proxy.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(e.id().clone()));
        }));

        self.menu = Some(menu);
        core.renderer = Some(renderer);
        core.workbench.set_window(window.clone());
        core.window = Some(window);
        core.apply_effects();
        core.redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => self.push(Input::Menu(id)),
            UserEvent::Exit => event_loop.exit(),
            UserEvent::Wake => {
                if let Ok(core) = self.shared.core.try_borrow() {
                    core.redraw();
                }
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                // Skipped while a dialog is open (its caller holds the core); `drain` redraws after.
                let Ok(mut core) = self.shared.core.try_borrow_mut() else { return };
                let core = &mut *core;
                if let Some(r) = &mut core.renderer {
                    // Going full screen doesn't always send Resized: draw at the window's size.
                    if let Some(w) = &core.window {
                        let size = w.inner_size();
                        if (size.width, size.height) != r.physical_size() {
                            r.resize(size.width, size.height, w.scale_factor() as f32);
                        }
                    }
                    let bg = core.workbench.background();
                    let wb = &mut core.workbench;
                    if r.frame(bg, |c| wb.draw(c)) {
                        core.redraw();
                    }
                }
                core.apply_effects();
            }
            // Dragging the window must start while AppKit is still handling the mouse-down
            // (it uses the current event), so title bar clicks are handled right away. They
            // never open dialogs.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
                if self.shared.queue.borrow().is_empty()
                    && self.shared.core.try_borrow().is_ok_and(|c| c.workbench.title_bar_at_mouse()) =>
            {
                let mut core = self.shared.core.borrow_mut();
                core.handle(Input::Window(event));
                core.apply_effects();
                core.redraw();
            }
            // macOS can show the window after the first frames were drawn (launched with
            // `open`): draw again once it's actually visible, or it stays blank.
            WindowEvent::Occluded(false) => {
                if let Ok(core) = self.shared.core.try_borrow() {
                    core.redraw();
                }
            }
            WindowEvent::MouseInput { button: MouseButton::Left | MouseButton::Right, .. }
            | WindowEvent::CloseRequested
            | WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. }
            | WindowEvent::ModifiersChanged(_)
            | WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::KeyboardInput { is_synthetic: false, .. }
            | WindowEvent::DroppedFile(_)
            | WindowEvent::Focused(_) => self.push(Input::Window(event)),
            _ => {}
        }
    }

    /// The app is ending: after Quit, or when macOS terminates it without asking us (logout,
    /// Dock "Quit"), in which case we save the session first. macOS then calls `exit()` from
    /// inside the run loop, so nothing is dropped normally: stop servers and shells here, and
    /// leave the window and GPU objects to the OS (tearing wgpu down during exit panics).
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Ok(mut core) = self.shared.core.try_borrow_mut() {
            if !core.quitting {
                core.workbench.terminating();
            }
            core.workbench.shutdown();
            std::mem::forget(core.renderer.take());
            std::mem::forget(core.window.take());
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Ok(core) = self.shared.core.try_borrow() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        match core.workbench.next_wakeup() {
            Some(t) if t <= Instant::now() => {
                core.redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            if let Ok(core) = self.shared.core.try_borrow() {
                core.redraw();
            }
        }
    }
}

fn main() {
    // git and ssh run us as their askpass helper (see `scm::askpass`): answer from the UI.
    if let Some(socket) = std::env::var_os(scm::askpass::SOCKET_VAR) {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        std::process::exit(scm::askpass::client(std::path::Path::new(&socket), &prompt));
    }
    // The agent runs us as the editor's MCP server (see `acp::mcp`): answer from the editor.
    if std::env::args().nth(1).as_deref() == Some(acp::mcp::HELPER_ARG) {
        if let Some(socket) = std::env::var_os(acp::mcp::SOCKET_VAR) {
            std::process::exit(acp::mcp::serve_stdio(std::path::Path::new(&socket), env!("CARGO_PKG_VERSION")));
        }
    }
    servers::extend_path();
    settings::migrate_user_data();
    log_panics();
    // Usage: orbvane [folder] [files...]
    let mut args = std::env::args_os().skip(1).map(PathBuf::from).map(|p| p.canonicalize().unwrap_or(p));
    let folder = args.next();
    let files: Vec<PathBuf> = args.collect();
    if let Some(out) = std::env::var_os("ORBVANE_SNAPSHOT") {
        snapshot(PathBuf::from(out), folder, files);
    }
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("failed to create event loop");
    let proxy = event_loop.create_proxy();
    let lsp_proxy = proxy.clone();
    let waker: lsp::Waker = std::sync::Arc::new(move || {
        let _ = lsp_proxy.send_event(UserEvent::Wake);
    });
    let core = Core {
        window: None,
        renderer: None,
        workbench: Workbench::new(folder, &files, waker),
        modifiers: ModifiersState::empty(),
        proxy,
        last_key_command: None,
        quitting: false,
    };
    let shared = Rc::new(Shared { core: RefCell::new(core), queue: RefCell::new(VecDeque::new()), scheduled: Cell::new(false) });
    SHARED.with(|s| *s.borrow_mut() = Some(shared.clone()));
    let mut app = App { shared, menu: None };
    event_loop.run_app(&mut app).expect("event loop error");
}

/// Development aid: draws the workbench offscreen and saves it as a PNG, then exits, for
/// checking the UI without a window (`ORBVANE_SNAPSHOT=out.png`). Options:
/// `ORBVANE_SNAPSHOT_SIZE=1400x900` (points; drawn at 2x), `ORBVANE_SNAPSHOT_WAIT=6` (seconds to
/// let servers and git answer first) and `ORBVANE_SNAPSHOT_COMMANDS=id,id` (commands run
/// halfway through, like `workbench.action.quickOpen`).
fn snapshot(out: PathBuf, folder: Option<PathBuf>, files: Vec<PathBuf>) -> ! {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (w, h) = var("ORBVANE_SNAPSHOT_SIZE")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
        })
        .unwrap_or((1400.0, 900.0));
    let wait = std::time::Duration::from_secs_f32(var("ORBVANE_SNAPSHOT_WAIT").and_then(|s| s.parse().ok()).unwrap_or(6.0));
    let commands: Vec<String> = var("ORBVANE_SNAPSHOT_COMMANDS").map(|s| s.split(',').map(|c| c.trim().to_string()).collect()).unwrap_or_default();
    let scale = 2.0;
    let mut wb = Workbench::new(folder, &files, std::sync::Arc::new(|| {}));
    let mut renderer = match render::Renderer::offscreen(((w * scale) as u32, (h * scale) as u32), scale) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("can't draw offscreen: {e}");
            std::process::exit(1);
        }
    };
    let start = std::time::Instant::now();
    let mut ran = commands.is_empty();
    loop {
        let bg = wb.background();
        renderer.frame(bg, |c| wb.draw(c));
        wb.take_effects();
        if !ran && start.elapsed() >= wait / 2 {
            ran = true;
            for id in &commands {
                match Command::from_id(id) {
                    Some(cmd) => wb.run(cmd),
                    None => eprintln!("no command {id}"),
                }
            }
        }
        if start.elapsed() >= wait {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let bg = wb.background();
    renderer.frame(bg, |c| wb.draw(c));
    let (pw, ph, pixels) = renderer.pixels();
    let code = match render::write_png(&out, pw, ph, pixels) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("can't write {}: {e}", out.display());
            1
        }
    };
    wb.shutdown();
    std::process::exit(code);
}

/// Appends panics (message, place and backtrace) to `<user data>/logs/panic.log`, since an
/// app started from Finder or `open` has nowhere to print them.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let dir = settings::user_data_dir().join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("panic.log")) else { return };
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        use std::io::Write;
        let _ = writeln!(file, "[{secs}] panic on thread '{thread}': {info}\n{backtrace}\n");
    }));
}
