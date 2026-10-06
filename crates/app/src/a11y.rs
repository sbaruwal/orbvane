//! Accessibility on macOS: our own bindings to AppKit's NSAccessibility. The workbench describes
//! what each window shows (`workbench::a11y`: areas and text areas, collected while drawing);
//! here each of those becomes an accessibility element (one class of ours, a subclass of
//! NSAccessibilityElement) that answers VoiceOver's questions by asking the window's workbench.
//! winit's view gets the methods that make them its children, and `after_frame` posts the
//! notifications for focus, text and selection changes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::null_mut;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Imp, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSArray, NSPoint, NSRange, NSRect, NSSize, NSString};
use render::Rect;

use crate::workbench::{A11yRole, A11yState, Workbench};

/// One accessibility element: which window and which of its nodes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    window: u64,
    node: u64,
}

thread_local! {
    /// Our element class, made on first use.
    static CLASS: Cell<Option<&'static AnyClass>> = const { Cell::new(None) };
    /// The elements made so far (they keep their identity while their node exists).
    static ELEMENTS: RefCell<HashMap<Key, Retained<AnyObject>>> = RefCell::new(HashMap::new());
    /// Element pointer → its key.
    static KEYS: RefCell<HashMap<usize, Key>> = RefCell::new(HashMap::new());
    /// winit's view of each window → the window.
    static VIEWS: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Whether winit's view class has our methods yet.
    static VIEW_METHODS: Cell<bool> = const { Cell::new(false) };
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn NSAccessibilityPostNotification(element: *mut AnyObject, notification: *mut NSString);
}

// ------------------------------------------------------------------ the workbench behind it

/// Runs `f` with window `window`'s workbench (None while the app state is busy, e.g. a dialog's
/// run loop asking during input handling, or when the window is gone).
fn with_workbench<R>(window: u64, f: impl FnOnce(&Workbench) -> R) -> Option<R> {
    let shared = crate::shared();
    let core = shared.core.try_borrow().ok()?;
    let w = core.wins.iter().find(|w| u64::from(w.window.id()) == window)?;
    Some(f(&w.workbench))
}

fn with_workbench_mut<R>(window: u64, f: impl FnOnce(&mut Workbench) -> R) -> Option<R> {
    let shared = crate::shared();
    let mut core = shared.core.try_borrow_mut().ok()?;
    let w = core.wins.iter_mut().find(|w| u64::from(w.window.id()) == window)?;
    w.workbench.activate();
    let r = f(&mut w.workbench);
    w.window.request_redraw();
    Some(r)
}

fn key_of(this: *const AnyObject) -> Option<Key> {
    KEYS.with(|k| k.borrow().get(&(this as usize)).copied())
}

fn view_of(window: u64) -> *mut AnyObject {
    VIEWS.with(|v| v.borrow().iter().find(|(_, w)| **w == window).map_or(null_mut(), |(p, _)| *p as *mut AnyObject))
}

/// The element for `node` of `window`, made the first time it's asked for.
fn element(window: u64, node: u64) -> *mut AnyObject {
    let key = Key { window, node };
    if let Some(e) = ELEMENTS.with(|e| e.borrow().get(&key).map(|e| Retained::as_ptr(e) as *mut AnyObject)) {
        return e;
    }
    let class = element_class();
    // SAFETY: `new` on an NSObject subclass returns a +1 object.
    let obj: Retained<AnyObject> = unsafe { msg_send![class, new] };
    let ptr = Retained::as_ptr(&obj) as *mut AnyObject;
    KEYS.with(|k| k.borrow_mut().insert(ptr as usize, key));
    ELEMENTS.with(|e| e.borrow_mut().insert(key, obj));
    ptr
}

fn ns_string(s: &str) -> *mut NSString {
    Retained::autorelease_return(NSString::from_str(s))
}

fn ns_array(items: &[*mut AnyObject]) -> *mut NSArray<AnyObject> {
    // SAFETY: the pointers are live elements (kept in ELEMENTS) or views.
    let objects: Vec<&AnyObject> = items.iter().map(|p| unsafe { &**p }).collect();
    Retained::autorelease_return(NSArray::from_slice(&objects))
}

fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(NSPoint::new(r.x as f64, r.y as f64), NSSize::new(r.w as f64, r.h as f64))
}

/// A rectangle in the window's (flipped, top-left) view coordinates, in screen coordinates.
fn to_screen(window: u64, r: Rect) -> NSRect {
    let view = view_of(window);
    if view.is_null() {
        return ns_rect(r);
    }
    // SAFETY: `view` is winit's live view; nil means the window's coordinates.
    unsafe {
        let nil: *mut AnyObject = null_mut();
        let in_window: NSRect = msg_send![view, convertRect: ns_rect(r), toView: nil];
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return in_window;
        }
        msg_send![win, convertRectToScreen: in_window]
    }
}

/// A point in screen coordinates, in the view's coordinates.
fn from_screen(window: u64, p: NSPoint) -> Option<(f32, f32)> {
    let view = view_of(window);
    if view.is_null() {
        return None;
    }
    // SAFETY: as in `to_screen`.
    unsafe {
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return None;
        }
        let in_window: NSPoint = msg_send![win, convertPointFromScreen: p];
        let nil: *mut AnyObject = null_mut();
        let local: NSPoint = msg_send![view, convertPoint: in_window, fromView: nil];
        Some((local.x as f32, local.y as f32))
    }
}

fn range(r: (usize, usize)) -> NSRange {
    NSRange::new(r.0, r.1)
}

// ------------------------------------------------------------------ the element class

/// Our NSAccessibilityElement subclass, registered once.
fn element_class() -> &'static AnyClass {
    if let Some(c) = CLASS.get() {
        return c;
    }
    let superclass = class!(NSAccessibilityElement);
    let mut b = ClassBuilder::new(c"OrbvaneAccessibilityElement", superclass).expect("class name taken");
    // SAFETY: each function's signature matches its selector's.
    unsafe {
        b.add_method(sel!(isAccessibilityElement), is_element as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityRole), role as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityLabel), label as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityFrame), frame as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityParent), parent as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityChildren), children as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(isAccessibilityFocused), is_focused as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityValue), value as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityNumberOfCharacters), char_count as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilitySelectedTextRange), selected_range as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(setAccessibilitySelectedTextRange:), set_selected_range as unsafe extern "C-unwind" fn(_, _, _));
        b.add_method(sel!(accessibilitySelectedText), selected_text as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityInsertionPointLineNumber), insertion_line as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityVisibleCharacterRange), visible_range as unsafe extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(accessibilityLineForIndex:), line_for_index as unsafe extern "C-unwind" fn(_, _, _) -> _);
        b.add_method(sel!(accessibilityRangeForLine:), range_for_line as unsafe extern "C-unwind" fn(_, _, _) -> _);
        b.add_method(sel!(accessibilityStringForRange:), string_for_range as unsafe extern "C-unwind" fn(_, _, _) -> _);
        b.add_method(sel!(accessibilityFrameForRange:), frame_for_range as unsafe extern "C-unwind" fn(_, _, _) -> _);
    }
    let class = b.register();
    CLASS.set(Some(class));
    class
}

fn node_role(key: Key) -> Option<A11yRole> {
    with_workbench(key.window, |wb| wb.a11y_node(key.node).map(|n| n.role)).flatten()
}

fn is_text(key: Key) -> bool {
    node_role(key) == Some(A11yRole::TextArea)
}

unsafe extern "C-unwind" fn is_element(_this: &AnyObject, _: Sel) -> Bool {
    Bool::YES
}

unsafe extern "C-unwind" fn role(this: &AnyObject, _: Sel) -> *mut NSString {
    let role = match key_of(this).and_then(node_role) {
        Some(A11yRole::TextArea) => "AXTextArea",
        _ => "AXGroup",
    };
    ns_string(role)
}

unsafe extern "C-unwind" fn label(this: &AnyObject, _: Sel) -> *mut NSString {
    let label = key_of(this).and_then(|k| with_workbench(k.window, |wb| wb.a11y_node(k.node).map(|n| n.label.clone())).flatten());
    ns_string(&label.unwrap_or_default())
}

unsafe extern "C-unwind" fn frame(this: &AnyObject, _: Sel) -> NSRect {
    let Some(k) = key_of(this) else { return NSRect::ZERO };
    let r = with_workbench(k.window, |wb| wb.a11y_node(k.node).map(|n| n.frame)).flatten().unwrap_or_default();
    to_screen(k.window, r)
}

unsafe extern "C-unwind" fn parent(this: &AnyObject, _: Sel) -> *mut AnyObject {
    let Some(k) = key_of(this) else { return null_mut() };
    match with_workbench(k.window, |wb| wb.a11y_node(k.node).and_then(|n| n.parent)).flatten() {
        Some(p) => element(k.window, p),
        None => view_of(k.window),
    }
}

unsafe extern "C-unwind" fn children(this: &AnyObject, _: Sel) -> *mut NSArray<AnyObject> {
    let Some(k) = key_of(this) else { return ns_array(&[]) };
    let ids = with_workbench(k.window, |wb| wb.a11y_children(Some(k.node))).unwrap_or_default();
    ns_array(&ids.into_iter().map(|id| element(k.window, id)).collect::<Vec<_>>())
}

unsafe extern "C-unwind" fn is_focused(this: &AnyObject, _: Sel) -> Bool {
    let focused = key_of(this).is_some_and(|k| with_workbench(k.window, |wb| wb.a11y_focused() == Some(k.node)).unwrap_or(false));
    Bool::new(focused)
}

/// Asks the workbench about a text area (nothing for a group).
fn text<R: Default>(this: &AnyObject, f: impl FnOnce(&Workbench, u64) -> R) -> R {
    match key_of(this).filter(|k| is_text(*k)) {
        Some(k) => with_workbench(k.window, |wb| f(wb, k.node)).unwrap_or_default(),
        None => R::default(),
    }
}

unsafe extern "C-unwind" fn value(this: &AnyObject, _: Sel) -> *mut AnyObject {
    match key_of(this).filter(|k| is_text(*k)) {
        Some(_) => ns_string(&text(this, |wb, id| wb.a11y_value(id))) as *mut AnyObject,
        None => null_mut(),
    }
}

unsafe extern "C-unwind" fn char_count(this: &AnyObject, _: Sel) -> isize {
    text(this, |wb, id| wb.a11y_length(id)) as isize
}

unsafe extern "C-unwind" fn selected_range(this: &AnyObject, _: Sel) -> NSRange {
    range(text(this, |wb, id| wb.a11y_selection(id)))
}

unsafe extern "C-unwind" fn set_selected_range(this: &AnyObject, _: Sel, r: NSRange) {
    if let Some(k) = key_of(this).filter(|k| is_text(*k)) {
        with_workbench_mut(k.window, |wb| wb.a11y_select(k.node, r.location, r.length));
    }
}

unsafe extern "C-unwind" fn selected_text(this: &AnyObject, _: Sel) -> *mut NSString {
    let s = text(this, |wb, id| {
        let (start, len) = wb.a11y_selection(id);
        wb.a11y_string(id, start, len)
    });
    ns_string(&s)
}

unsafe extern "C-unwind" fn insertion_line(this: &AnyObject, _: Sel) -> isize {
    text(this, |wb, id| {
        let (start, len) = wb.a11y_selection(id);
        wb.a11y_line_of(id, start + len)
    }) as isize
}

unsafe extern "C-unwind" fn visible_range(this: &AnyObject, _: Sel) -> NSRange {
    range(text(this, |wb, id| wb.a11y_visible(id)))
}

unsafe extern "C-unwind" fn line_for_index(this: &AnyObject, _: Sel, index: isize) -> isize {
    text(this, |wb, id| wb.a11y_line_of(id, index.max(0) as usize)) as isize
}

unsafe extern "C-unwind" fn range_for_line(this: &AnyObject, _: Sel, line: isize) -> NSRange {
    range(text(this, |wb, id| wb.a11y_line_range(id, line.max(0) as usize)))
}

unsafe extern "C-unwind" fn string_for_range(this: &AnyObject, _: Sel, r: NSRange) -> *mut NSString {
    ns_string(&text(this, |wb, id| wb.a11y_string(id, r.location, r.length)))
}

unsafe extern "C-unwind" fn frame_for_range(this: &AnyObject, _: Sel, r: NSRange) -> NSRect {
    let Some(k) = key_of(this).filter(|k| is_text(*k)) else { return NSRect::ZERO };
    let local = with_workbench(k.window, |wb| wb.a11y_range_frame(k.node, r.location, r.length)).unwrap_or_default();
    to_screen(k.window, local)
}

// ------------------------------------------------------------------ winit's view

/// The view's children: the window's areas.
unsafe extern "C-unwind" fn view_children(this: *mut AnyObject, _: Sel) -> *mut NSArray<AnyObject> {
    let Some(window) = VIEWS.with(|v| v.borrow().get(&(this as usize)).copied()) else { return ns_array(&[]) };
    let ids = with_workbench(window, |wb| wb.a11y_children(None)).unwrap_or_default();
    ns_array(&ids.into_iter().map(|id| element(window, id)).collect::<Vec<_>>())
}

/// What has the keyboard: the editor's text area, else the view itself.
unsafe extern "C-unwind" fn view_focused(this: *mut AnyObject, _: Sel) -> *mut AnyObject {
    let Some(window) = VIEWS.with(|v| v.borrow().get(&(this as usize)).copied()) else { return this };
    match with_workbench(window, |wb| wb.a11y_focused()).flatten() {
        Some(id) => element(window, id),
        None => this,
    }
}

/// The deepest element under a screen point (text areas before the areas around them).
unsafe extern "C-unwind" fn view_hit_test(this: *mut AnyObject, _: Sel, p: NSPoint) -> *mut AnyObject {
    let Some(window) = VIEWS.with(|v| v.borrow().get(&(this as usize)).copied()) else { return this };
    let Some((x, y)) = from_screen(window, p) else { return this };
    let hit = with_workbench(window, |wb| {
        let inside = |id: &u64| wb.a11y_node(*id).is_some_and(|n| n.frame.contains(x, y));
        let area = wb.a11y_children(None).into_iter().find(inside)?;
        Some(wb.a11y_children(Some(area)).into_iter().find(inside).unwrap_or(area))
    })
    .flatten();
    match hit {
        Some(id) => element(window, id),
        None => this,
    }
}

type ObjFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;
type ArrayFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut NSArray<AnyObject>;
type HitFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, NSPoint) -> *mut AnyObject;

/// Makes `window`'s view (winit's) the parent of its accessibility elements. The methods are
/// added to winit's view class once; each window's view is remembered here.
pub fn attach(window: &winit::window::Window) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view = h.ns_view.as_ptr() as *mut AnyObject;
    VIEWS.with(|v| v.borrow_mut().insert(view as usize, u64::from(window.id())));
    if VIEW_METHODS.replace(true) {
        return;
    }
    // SAFETY: main thread; the type strings match the functions ("@@:" returns an object and
    // takes self and _cmd; the hit test also takes an NSPoint).
    unsafe {
        let class = (*view).class() as *const AnyClass as *mut AnyClass;
        let add = |sel: Sel, imp: Imp, types: &std::ffi::CStr| {
            objc2::ffi::class_addMethod(class, sel, imp, types.as_ptr());
        };
        add(sel!(accessibilityChildren), std::mem::transmute::<ArrayFn, Imp>(view_children), c"@@:");
        add(sel!(accessibilityFocusedUIElement), std::mem::transmute::<ObjFn, Imp>(view_focused), c"@@:");
        add(sel!(accessibilityHitTest:), std::mem::transmute::<HitFn, Imp>(view_hit_test), c"@@:{CGPoint=dd}");
    }
}

/// A window closed: forget its view and elements.
pub fn detach(window: winit::window::WindowId) {
    let id = u64::from(window);
    VIEWS.with(|v| v.borrow_mut().retain(|_, w| *w != id));
    let gone: Vec<Retained<AnyObject>> = ELEMENTS.with(|e| {
        let mut e = e.borrow_mut();
        let keys: Vec<Key> = e.keys().filter(|k| k.window == id).copied().collect();
        keys.into_iter().filter_map(|k| e.remove(&k)).collect()
    });
    KEYS.with(|k| k.borrow_mut().retain(|_, key| key.window != id));
    drop(gone);
}

fn post(element: *mut AnyObject, name: &str) {
    if element.is_null() {
        return;
    }
    let name = NSString::from_str(name);
    // SAFETY: a live element and a notification name.
    unsafe { NSAccessibilityPostNotification(element, Retained::as_ptr(&name) as *mut NSString) };
}

/// After a frame: tells assistive technology what changed since `last` (focus, the focused
/// text, its selection).
pub fn after_frame(window: winit::window::WindowId, wb: &Workbench, last: &mut A11yState) {
    let now = wb.a11y_state();
    if now == *last {
        return;
    }
    let id = u64::from(window);
    match now.focused {
        Some(node) => {
            let e = element(id, node);
            if last.focused != now.focused {
                post(e, "AXFocusedUIElementChanged");
            } else {
                if last.version != now.version {
                    post(e, "AXValueChanged");
                }
                if last.selection != now.selection {
                    post(e, "AXSelectedTextChanged");
                }
            }
        }
        None if last.focused.is_some() => post(view_of(id), "AXFocusedUIElementChanged"),
        None => {}
    }
    *last = now;
}
