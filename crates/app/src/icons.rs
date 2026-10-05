//! The workbench's icon set: our own line icons on a 16×16 grid, drawn in a consistent outline style.

use render::Icon;

const fn stroke(path: &'static str) -> Icon {
    Icon { path, viewbox: 16.0, stroke: Some(1.0) }
}

const fn bold(path: &'static str) -> Icon {
    Icon { path, viewbox: 16.0, stroke: Some(1.4) }
}

pub const FILES: Icon = stroke(
    "M5.5 3.5V1.5H10.5L13.5 4.5V11.5H10.5 M2.5 4.5H7.5L10.5 7.5V14.5H2.5Z M7.5 4.5V7.5H10.5 M10.5 1.5V4.5H13.5",
);
pub const SEARCH: Icon = stroke("M14 6.5A4.5 4.5 0 1 1 5 6.5A4.5 4.5 0 1 1 14 6.5Z M6.3 9.7L2 14");
pub const SOURCE_CONTROL: Icon = stroke(
    "M6 3A1.5 1.5 0 1 1 3 3A1.5 1.5 0 1 1 6 3Z M6 13A1.5 1.5 0 1 1 3 13A1.5 1.5 0 1 1 6 13Z \
     M13 5A1.5 1.5 0 1 1 10 5A1.5 1.5 0 1 1 13 5Z M4.5 4.5V11.5 M11.5 6.5C11.5 9.5 4.5 8.5 4.5 11.5",
);
pub const RUN_DEBUG: Icon = stroke(
    "M6.5 2.5L14 8L6.5 13.5Z M5 12A2.5 2.5 0 1 1 0 12 M2.5 9.5V8 M0.8 10L0 9.3",
);
pub const EXTENSIONS: Icon = stroke(
    "M2.5 6.5H6.5V10.5H2.5Z M2.5 10.5H6.5V14.5H2.5Z M6.5 10.5H10.5V14.5H6.5Z M9.5 1.5H14.5V6.5H9.5Z",
);
pub const ACCOUNT: Icon =
    stroke("M11 5A3 3 0 1 1 5 5A3 3 0 1 1 11 5Z M2.5 14.5C2.5 11.5 5 9.5 8 9.5S13.5 11.5 13.5 14.5");
pub const GEAR: Icon = stroke(
    "M10 8A2 2 0 1 1 6 8A2 2 0 1 1 10 8Z M12.5 8A4.5 4.5 0 1 1 3.5 8A4.5 4.5 0 1 1 12.5 8Z \
     M8 1.5V3.5 M8 12.5V14.5 M1.5 8H3.5 M12.5 8H14.5 M3.4 3.4L4.8 4.8 M11.2 11.2L12.6 12.6 \
     M3.4 12.6L4.8 11.2 M11.2 4.8L12.6 3.4",
);
/// `color-mode`: a circle, half of it filled.
pub const COLOR_MODE: Icon = stroke("M13.5 8A5.5 5.5 0 1 1 2.5 8A5.5 5.5 0 1 1 13.5 8Z M8 2.5V13.5 M8 4.5A3.5 3.5 0 0 1 8 11.5 M8 6.5A1.5 1.5 0 0 1 8 9.5");
pub const CHEVRON_RIGHT: Icon = stroke("M6 3.5L10.5 8L6 12.5");
pub const CHEVRON_DOWN: Icon = stroke("M3.5 6L8 10.5L12.5 6");
pub const CHEVRON_UP: Icon = stroke("M3.5 10L8 5.5L12.5 10");
/// The code action lightbulb (`light-bulb`), and with a spark when a preferred fix exists
/// (`lightbulb-autofix`).
pub const LIGHT_BULB: Icon = stroke("M8 1.5A4.5 4.5 0 0 0 5.5 9.7V11.5H10.5V9.7A4.5 4.5 0 0 0 8 1.5Z M6.5 13.5H9.5");
pub const LIGHT_BULB_AUTOFIX: Icon =
    stroke("M7 1.5A4.5 4.5 0 0 0 4.5 9.7V11.5H9.5V9.7A4.5 4.5 0 0 0 7 1.5Z M5.5 13.5H8.5 M13 10.5V14.5 M11 12.5H15");
pub const CLOSE: Icon = stroke("M4 4L12 12 M12 4L4 12");
pub const DOT: Icon = Icon { path: "M10 8A2 2 0 1 1 6 8A2 2 0 1 1 10 8Z", viewbox: 16.0, stroke: Some(2.6) };
pub const ELLIPSIS: Icon = Icon {
    path: "M4 8A.3 .3 0 1 1 3.4 8A.3 .3 0 1 1 4 8Z M8.3 8A.3 .3 0 1 1 7.7 8A.3 .3 0 1 1 8.3 8Z \
           M12.6 8A.3 .3 0 1 1 12 8A.3 .3 0 1 1 12.6 8Z",
    viewbox: 16.0,
    stroke: Some(1.6),
};
pub const FOLDER: Icon = stroke("M1.5 3.5H6L7.5 5H14.5V13.5H1.5Z");
pub const FILE: Icon = stroke("M3.5 1.5H9.5L12.5 4.5V14.5H3.5Z M9.5 1.5V4.5H12.5");
pub const REMOTE: Icon = bold("M2.5 4L6 7.5L2.5 11 M13.5 5L10 8.5L13.5 12");
pub const BRANCH: Icon = stroke(
    "M6 3A1.5 1.5 0 1 1 3 3A1.5 1.5 0 1 1 6 3Z M6 13A1.5 1.5 0 1 1 3 13A1.5 1.5 0 1 1 6 13Z \
     M13 4A1.5 1.5 0 1 1 10 4A1.5 1.5 0 1 1 13 4Z M4.5 4.5V11.5 M11.5 5.5C11.5 9 4.5 8 4.5 11.5",
);
pub const ERROR: Icon =
    stroke("M14 8A6 6 0 1 1 2 8A6 6 0 1 1 14 8Z M5.9 5.9L10.1 10.1 M10.1 5.9L5.9 10.1");
pub const WARNING: Icon = stroke("M8 1.8L14.7 13.7H1.3Z M8 6V9.8 M8 11.6V11.8");
pub const BELL: Icon = stroke("M4 11V7A4 4 0 0 1 12 7V11L13.5 12.5H2.5Z M6.5 14.5H9.5");
pub const SPLIT: Icon = stroke("M2.5 2.5H13.5V13.5H2.5Z M8 2.5V13.5");
pub const LAYOUT_SIDEBAR: Icon = stroke("M1.5 2.5H14.5V13.5H1.5Z M6 2.5V13.5");
pub const LAYOUT_PANEL: Icon = stroke("M1.5 2.5H14.5V13.5H1.5Z M1.5 9.5H14.5");
pub const LAYOUT_SIDEBAR_RIGHT: Icon = stroke("M1.5 2.5H14.5V13.5H1.5Z M10 2.5V13.5");
pub const ARROW_LEFT: Icon = stroke("M13.5 8H2.5 M6.5 4L2.5 8L6.5 12");
pub const ARROW_RIGHT: Icon = stroke("M2.5 8H13.5 M9.5 4L13.5 8L9.5 12");

// Symbol kinds for the suggest widget.
pub const SYMBOL_METHOD: Icon = stroke("M8 1.8L13.5 4.9V11.1L8 14.2L2.5 11.1V4.9Z M2.5 4.9L8 8L13.5 4.9 M8 8V14.2");
pub const SYMBOL_FIELD: Icon = stroke("M2.5 5.5L8 2.5L13.5 5.5V10.5L8 13.5L2.5 10.5Z M8 8.5V13.5 M2.5 5.5L8 8.5L13.5 5.5");
pub const SYMBOL_CLASS: Icon = stroke("M4 3.5H8V7.5H4Z M9.5 9.5H13.5V13.5H9.5Z M6 7.5V11.5H9.5 M8 5.5H11.5V9.5");
pub const SYMBOL_VARIABLE: Icon = stroke("M4.5 3.5H2.5V12.5H4.5 M11.5 3.5H13.5V12.5H11.5 M5.5 6L10.5 10 M10.5 6L5.5 10");
pub const SYMBOL_KEYWORD: Icon = stroke("M2.5 5.5H13.5 M2.5 8.5H9.5 M11 8.5H13.5 M2.5 11.5H6.5 M8 11.5H13.5");
pub const SYMBOL_STRUCT: Icon = stroke("M2.5 2.5H13.5V6.5H2.5Z M2.5 9.5H6.5V13.5H2.5Z M9.5 9.5H13.5V13.5H9.5Z");
pub const SYMBOL_ENUM: Icon = stroke("M2.5 5.5H9.5V13.5H2.5Z M6.5 5.5V2.5H13.5V10.5H9.5 M4.5 8.5H7.5 M4.5 10.5H7.5");
pub const SYMBOL_ENUM_MEMBER: Icon = stroke("M2.5 2.5H8.5V8.5H2.5Z M8.5 6.5H13.5V13.5H6.5V8.5 M4 5.5H7");
pub const SYMBOL_INTERFACE: Icon = stroke("M7.5 8.5A2.5 2.5 0 1 1 2.5 8.5A2.5 2.5 0 1 1 7.5 8.5Z M7.5 8.5H13.5");
pub const SYMBOL_CONSTANT: Icon = stroke("M2.5 4.5H13.5V11.5H2.5Z M5 7H11 M5 9H11");
pub const SYMBOL_PROPERTY: Icon = stroke("M13 3.5A3 3 0 0 1 9.2 7.3L3.8 12.7A1 1 0 0 1 2.3 11.2L7.7 5.8A3 3 0 0 1 11.5 2L9.8 3.7L10.5 5.5L12.3 6.2Z");
pub const SYMBOL_EVENT: Icon = stroke("M9.5 1.5L4.5 9H8L6.5 14.5L11.5 7H8Z");
pub const GIT_COMMIT: Icon = stroke("M5.5 8A2.5 2.5 0 1 0 10.5 8A2.5 2.5 0 1 0 5.5 8Z M1.5 8H5.5 M10.5 8H14.5");
pub const SYMBOL_MODULE: Icon = stroke("M5 2.5C3.5 2.5 3.5 3.5 3.5 5V6.5C3.5 7.5 2.5 8 2 8C2.5 8 3.5 8.5 3.5 9.5V11C3.5 12.5 3.5 13.5 5 13.5 M11 2.5C12.5 2.5 12.5 3.5 12.5 5V6.5C12.5 7.5 13.5 8 14 8C13.5 8 12.5 8.5 12.5 9.5V11C12.5 12.5 12.5 13.5 11 13.5");
pub const INFO: Icon = stroke("M14 8A6 6 0 1 1 2 8A6 6 0 1 1 14 8Z M8 7.2V11 M8 4.9V5.1");
pub const ADD: Icon = stroke("M8 2.5V13.5 M2.5 8H13.5");
pub const TRASH: Icon = stroke("M2.5 4H13.5 M6 4V2.5H10V4 M4 4L4.8 13.5H11.2L12 4 M6.7 6.5V11 M9.3 6.5V11");
pub const TERMINAL: Icon = stroke("M1.5 2.5H14.5V13.5H1.5Z M4 6L6.5 8.5L4 11 M8 11H12");
pub const REFRESH: Icon = stroke("M13 8A5 5 0 1 1 11.5 4.3 M11.8 1.5V4.5H8.8");
pub const CLEAR_ALL: Icon = stroke("M2.5 4.5H11 M2.5 7.5H8.5 M2.5 10.5H6.5 M9.5 9.5L13.5 13.5 M13.5 9.5L9.5 13.5");
pub const COLLAPSE_ALL: Icon = stroke("M4.5 2.5H13.5V11.5 M2.5 4.5H11.5V13.5H2.5Z M5 9H9");
pub const ARROW_UP: Icon = stroke("M8 13.5V2.5 M4 6.5L8 2.5L12 6.5");
pub const ARROW_DOWN: Icon = stroke("M8 2.5V13.5 M4 9.5L8 13.5L12 9.5");
pub const REPLACE: Icon = stroke("M2.5 2.5H6.5V6.5H2.5Z M9.5 9.5H13.5V13.5H9.5Z M4.5 8.5V11.5H8 M6.5 10L8 11.5L6.5 13");
pub const REPLACE_ALL: Icon = stroke(
    "M2.5 2.5H6.5V6.5H2.5Z M2.5 9.5H6.5V13.5H2.5Z M8.5 4.5H13.5 M11.5 2.5L13.5 4.5L11.5 6.5 M8.5 11.5H13.5 M11.5 9.5L13.5 11.5L11.5 13.5",
);
pub const CHECK: Icon = stroke("M2.5 8.5L6 12L13.5 4");
pub const CHECK_ALL: Icon = stroke("M1 8.5L4.5 12L12 4 M7 11L8 12L15.5 4");
pub const REMOVE: Icon = stroke("M3 8H13");
pub const DISCARD: Icon = stroke("M5.5 2.5L2.5 5.5L5.5 8.5 M2.5 5.5H10A3.5 3.5 0 0 1 10 12.5H7");
pub const GO_TO_FILE: Icon = stroke("M3.5 1.5H9.5L12.5 4.5V14.5H3.5Z M9.5 1.5V4.5H12.5 M6 9.5H10 M8.5 8L10 9.5L8.5 11");
/// Icon `settings`: the Settings editor tab.
pub const SETTINGS: Icon = stroke("M2 4H14 M2 8H14 M2 12H14 M5 2.5V5.5 M10.5 6.5V9.5 M6.5 10.5V13.5");
pub const SYNC: Icon = stroke("M13 8A5 5 0 0 1 4.2 11.2 M3 8A5 5 0 0 1 11.8 4.8 M11.8 2V4.8H9 M4.2 14V11.2H7");
pub const CLOUD_UPLOAD: Icon = stroke(
    "M5 12.5H4A3 3 0 0 1 3.8 6.5A4.3 4.3 0 0 1 12 6A3.2 3.2 0 0 1 12 12.5H11 M8 14V8 M5.8 10.2L8 8L10.2 10.2",
);
pub const CLOUD_DOWNLOAD: Icon = stroke(
    "M5 11.5H4A3 3 0 0 1 3.8 5.5A4.3 4.3 0 0 1 12 5A3.2 3.2 0 0 1 12 11.5H11 M8 8V14 M5.8 11.8L8 14L10.2 11.8",
);
// The marketplace's ratings and verified publishers.
pub const STAR_FULL: Icon = Icon {
    path: "M8 1.5L9.9 5.6L14.3 6.1L11 9.1L11.9 13.5L8 11.3L4.1 13.5L5 9.1L1.7 6.1L6.1 5.6Z",
    viewbox: 16.0,
    stroke: None,
};
pub const STAR_HALF: Icon = Icon { path: "M8 1.5V11.3L4.1 13.5L5 9.1L1.7 6.1L6.1 5.6Z", viewbox: 16.0, stroke: None };
pub const STAR_EMPTY: Icon = stroke("M8 1.5L9.9 5.6L14.3 6.1L11 9.1L11.9 13.5L8 11.3L4.1 13.5L5 9.1L1.7 6.1L6.1 5.6Z");
pub const VERIFIED: Icon = Icon {
    path: "M8 1L9.8 2.4L12.1 2.3L12.8 4.5L14.6 5.9L13.9 8L14.6 10.1L12.8 11.5L12.1 13.7L9.8 13.6L8 15L6.2 13.6L3.9 13.7L3.2 11.5L1.4 10.1L2.1 8L1.4 5.9L3.2 4.5L3.9 2.3L6.2 2.4Z",
    viewbox: 16.0,
    stroke: None,
};

// Debugging (the debug toolbar, Run and Debug view and glyph margin).
pub const DEBUG_START: Icon = Icon { path: "M4.5 2.5L13 8L4.5 13.5Z", viewbox: 16.0, stroke: None };
pub const DEBUG_CONTINUE: Icon = stroke("M3.5 2.5V13.5 M6.5 2.5L13.5 8L6.5 13.5Z");
pub const DEBUG_PAUSE: Icon = bold("M5 3V13 M11 3V13");
pub const DEBUG_STEP_OVER: Icon = stroke("M2.5 9A5.5 5.5 0 0 1 13 6.5 M13.5 2.5V6.5H9.5 M9.5 13A1.5 1.5 0 1 1 6.5 13A1.5 1.5 0 1 1 9.5 13Z");
pub const DEBUG_STEP_INTO: Icon = stroke("M8 1.5V9 M4.5 5.5L8 9L11.5 5.5 M9.5 13A1.5 1.5 0 1 1 6.5 13A1.5 1.5 0 1 1 9.5 13Z");
pub const DEBUG_STEP_OUT: Icon = stroke("M8 9V1.5 M4.5 5L8 1.5L11.5 5 M9.5 13A1.5 1.5 0 1 1 6.5 13A1.5 1.5 0 1 1 9.5 13Z");
pub const DEBUG_RESTART: Icon = stroke("M13 8A5 5 0 1 1 11.5 4.3 M11.8 1.5V4.5H8.8");
pub const DEBUG_STOP: Icon = stroke("M3.5 3.5H12.5V12.5H3.5Z");
/// The toolbar's drag handle.
pub const GRIPPER: Icon = bold("M6 4V4.2 M10 4V4.2 M6 8V8.2 M10 8V8.2 M6 12V12.2 M10 12V12.2");
/// The focused stack frame's arrow in the glyph margin.
pub const STACK_FRAME: Icon = Icon { path: "M1.5 5.5H8V2.5L14 8L8 13.5V10.5H1.5Z", viewbox: 16.0, stroke: None };
/// A logpoint (a diamond) in the glyph margin.
pub const LOGPOINT: Icon = Icon { path: "M8 3L13 8L8 13L3 8Z", viewbox: 16.0, stroke: None };
pub const BREAKPOINTS_ACTIVATE: Icon = stroke("M10.5 8A4 4 0 1 1 2.5 8A4 4 0 1 1 10.5 8Z M14 3.5V12.5");
pub const CLOSE_ALL: Icon = stroke("M5.5 2.5H13.5V10.5 M2.5 5.5H10.5V13.5H2.5Z M4.5 7.5L8.5 11.5 M8.5 7.5L4.5 11.5");
pub const EDIT: Icon = stroke("M10.5 2.5L13.5 5.5L5.5 13.5H2.5V10.5Z M9 4L12 7");
pub const NEW_FILE: Icon = stroke("M9.5 1.5H3.5V14.5H12.5V4.5L9.5 1.5Z M9.5 1.5V4.5H12.5 M8 7V12 M5.5 9.5H10.5");
pub const NEW_FOLDER: Icon = stroke("M1.5 3.5H6L7.5 5H14.5V13.5H1.5Z M8 7V12 M5.5 9.5H10.5");
/// Open Preview to the Side (`open-preview`): a split page with a magnifier.
pub const OPEN_PREVIEW: Icon = stroke("M1.5 2.5H14.5V13.5H1.5Z M8 2.5V13.5 M12.5 9A1.5 1.5 0 1 1 9.5 9A1.5 1.5 0 1 1 12.5 9Z M12 10.2L13.5 11.7");
pub const FILTER: Icon = stroke("M1.5 2.5H14.5L9.5 8.5V13.5L6.5 12V8.5Z");
/// Icon `beaker`: the Testing view.
pub const BEAKER: Icon = stroke("M6 1.5V6.2L2.4 12.9A1.1 1.1 0 0 0 3.4 14.5H12.6A1.1 1.1 0 0 0 13.6 12.9L10 6.2V1.5 M4.8 1.5H11.2 M4.3 10.5H11.7");
/// Test states: `pass`, `circle-outline` (unset), `history`
/// (queued); failed uses `ERROR`, skipped `DEBUG_STEP_OVER`.
pub const PASS: Icon = stroke("M14 8A6 6 0 1 1 2 8A6 6 0 1 1 14 8Z M5.2 8.2L7.2 10.2L10.9 6.3");
pub const CIRCLE_OUTLINE: Icon = stroke("M13 8A5 5 0 1 1 3 8A5 5 0 1 1 13 8Z");
pub const HISTORY: Icon = stroke("M2.9 9.5A5.5 5.5 0 1 0 4.2 4 M4.2 1.5V4.3H7 M8 5V8.3L10.3 9.8");
/// Icon `list-selection`: the search editor's context lines toggle.
pub const LIST_SELECTION: Icon = stroke("M2 3.5H14 M2 12.5H14 M1.5 6.5H14.5V9.5H1.5Z");
/// Icons `run` and `run-all`.
pub const RUN: Icon = stroke("M4.5 2.5L13 8L4.5 13.5Z");
pub const RUN_ALL: Icon = stroke("M2.5 3L8.5 8L2.5 13Z M8 3.4L14 8L8 12.6");

/// The icon and color key a test state is drawn with (`testingStatesToIcons`;
/// `run` for a test without results, where the gutter offers to run it).
pub fn test_state(state: crate::testing::TestState) -> (&'static Icon, &'static str) {
    use crate::testing::TestState;
    match state {
        TestState::Unset => (&CIRCLE_OUTLINE, "testing.iconUnset"),
        TestState::Queued => (&HISTORY, "testing.iconQueued"),
        TestState::Running => (&CIRCLE_OUTLINE, "testing.iconUnset"),
        TestState::Passed => (&PASS, "testing.iconPassed"),
        TestState::Failed => (&ERROR, "testing.iconFailed"),
        TestState::Errored => (&ERROR, "testing.iconErrored"),
        TestState::Skipped => (&DEBUG_STEP_OVER, "testing.iconSkipped"),
    }
}

/// A spinning "loading" mark centered in `r`: eight dots fading
/// around a circle. Callers keep frames coming while it shows.
pub fn draw_spinner(c: &mut render::Canvas, r: render::Rect, size: f32, color: render::Color) {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let t = START.get_or_init(std::time::Instant::now).elapsed().as_secs_f32();
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let radius = size * 0.36;
    let d = (size * 0.16).max(1.5);
    let head = (t * 8.0).floor() as i32;
    for i in 0..8 {
        let a = i as f32 / 8.0 * std::f32::consts::TAU;
        let age = (head - i).rem_euclid(8) as f32;
        let mut col = color;
        col.a *= 1.0 - age / 9.0;
        let (x, y) = (cx + a.sin() * radius, cy - a.cos() * radius);
        c.fill_rounded(render::Rect::new(x - d / 2.0, y - d / 2.0, d, d), col, d / 2.0);
    }
}

/// The icon for an icon name (`$(check)` in an extension's status bar text), if we have it.
/// An icon from an SVG file (an extension's): its `<path d="...">`s filled, in its `viewBox`.
/// Leaked, like the built-in icons are static; callers cache them.
pub fn from_svg(svg: &str) -> Option<&'static Icon> {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let at = tag.find(&format!(" {name}="))? + name.len() + 2;
        let quote = tag[at..].chars().next()?;
        let rest = &tag[at + 1..];
        Some(rest[..rest.find(quote)?].to_string())
    };
    let root_end = svg.find("<svg")?;
    let root = &svg[root_end..root_end + svg[root_end..].find('>')?];
    let viewbox = attr(root, "viewBox")
        .and_then(|v| {
            let n: Vec<f32> = v.split([' ', ',']).filter_map(|x| x.trim().parse().ok()).collect();
            (n.len() == 4).then(|| n[2].max(n[3]))
        })
        .or_else(|| attr(root, "width").and_then(|w| w.trim_end_matches("px").parse().ok()))
        .unwrap_or(16.0);
    let mut paths = Vec::new();
    let mut rest = svg;
    while let Some(i) = rest.find("<path") {
        let end = rest[i..].find('>').map_or(rest.len(), |e| i + e);
        if let Some(d) = attr(&rest[i..end], "d") {
            paths.push(d);
        }
        rest = &rest[end..];
    }
    if paths.is_empty() {
        return None;
    }
    let path: &'static str = Box::leak(paths.join(" ").into_boxed_str());
    Some(Box::leak(Box::new(Icon { path, viewbox, stroke: None })))
}

pub fn named(name: &str) -> Option<&'static Icon> {
    Some(match name {
        "files" | "file-code" => &FILES,
        "file" | "file-text" => &FILE,
        "search" => &SEARCH,
        "source-control" | "git-merge" => &SOURCE_CONTROL,
        "debug-alt" | "debug" => &RUN_DEBUG,
        "extensions" => &EXTENSIONS,
        "account" | "person" => &ACCOUNT,
        "gear" | "settings-gear" => &GEAR,
        "settings" => &SETTINGS,
        "chevron-right" => &CHEVRON_RIGHT,
        "chevron-down" => &CHEVRON_DOWN,
        "chevron-up" => &CHEVRON_UP,
        "lightbulb" => &LIGHT_BULB,
        "close" | "x" => &CLOSE,
        "ellipsis" | "kebab-horizontal" => &ELLIPSIS,
        "remote" => &REMOTE,
        "git-branch" => &BRANCH,
        "git-commit" => &GIT_COMMIT,
        "error" | "circle-slash" => &ERROR,
        "warning" | "alert" => &WARNING,
        "bell" => &BELL,
        "info" => &INFO,
        "add" | "plus" => &ADD,
        "trash" => &TRASH,
        "terminal" => &TERMINAL,
        "refresh" => &REFRESH,
        "check" => &CHECK,
        "star-full" => &STAR_FULL,
        "star-empty" => &STAR_EMPTY,
        "verified" | "verified-filled" => &VERIFIED,
        "cloud-download" => &CLOUD_DOWNLOAD,
        "check-all" => &CHECK_ALL,
        "sync" | "loading" => &SYNC,
        "cloud-upload" => &CLOUD_UPLOAD,
        "edit" | "pencil" => &EDIT,
        "new-file" => &NEW_FILE,
        "new-folder" => &NEW_FOLDER,
        "filter" => &FILTER,
        "beaker" => &BEAKER,
        "pass" | "pass-filled" => &PASS,
        "circle-outline" | "circle-large-outline" => &CIRCLE_OUTLINE,
        "history" => &HISTORY,
        "run" | "play" => &RUN,
        "run-all" => &RUN_ALL,
        "debug-start" => &DEBUG_START,
        "debug-stop" => &DEBUG_STOP,
        "arrow-up" => &ARROW_UP,
        "arrow-down" => &ARROW_DOWN,
        "arrow-left" => &ARROW_LEFT,
        "arrow-right" => &ARROW_RIGHT,
        "symbol-method" | "symbol-function" => &SYMBOL_METHOD,
        "symbol-class" => &SYMBOL_CLASS,
        "symbol-variable" => &SYMBOL_VARIABLE,
        "symbol-keyword" => &SYMBOL_KEYWORD,
        "symbol-field" => &SYMBOL_FIELD,
        "symbol-struct" => &SYMBOL_STRUCT,
        "symbol-enum" => &SYMBOL_ENUM,
        "symbol-enum-member" => &SYMBOL_ENUM_MEMBER,
        "symbol-interface" => &SYMBOL_INTERFACE,
        "symbol-constant" => &SYMBOL_CONSTANT,
        "symbol-property" => &SYMBOL_PROPERTY,
        "symbol-event" => &SYMBOL_EVENT,
        "symbol-module" | "symbol-namespace" | "symbol-package" => &SYMBOL_MODULE,
        "collapse-all" => &COLLAPSE_ALL,
        "clear-all" => &CLEAR_ALL,
        "close-all" => &CLOSE_ALL,
        "discard" => &DISCARD,
        "remove" => &REMOVE,
        "replace" => &REPLACE,
        "replace-all" => &REPLACE_ALL,
        "go-to-file" => &GO_TO_FILE,
        "list-selection" | "list-flat" | "list-tree" => &LIST_SELECTION,
        "circle-filled" | "dot" | "primitive-dot" => &DOT,
        "split-horizontal" => &SPLIT,
        "color-mode" => &COLOR_MODE,
        "debug-continue" => &DEBUG_CONTINUE,
        "debug-pause" => &DEBUG_PAUSE,
        "debug-restart" => &DEBUG_RESTART,
        "open-preview" => &OPEN_PREVIEW,
        "star-half" => &STAR_HALF,
        _ => return None,
    })
}
