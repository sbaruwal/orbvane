//! Settings the drawing and editing code reads constantly (font metrics, tab size, cursor
//! style...), resolved from the settings store into a small `Copy` struct. The workbench
//! refreshes it whenever settings change; everything runs on the main thread.

use std::cell::Cell;

use settings::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    Line,
    Block,
    Underline,
    LineThin,
    BlockOutline,
    UnderlineThin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineNumbers {
    Off,
    On,
    Relative,
    Interval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineHighlight {
    None,
    Gutter,
    Line,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whitespace {
    None,
    Boundary,
    Selection,
    Trailing,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoClose {
    Always,
    BeforeWhitespace,
    Never,
}

/// `editor.wordWrap`: None, or the wrap column (0: the viewport width) and whether the
/// viewport also bounds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordWrap {
    Off,
    Viewport,
    Column(usize),
    Bounded(usize),
}

/// `editor.showFoldingControls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldControls {
    Always,
    Never,
    MouseOver,
}

/// `emmet.showExpandedAbbreviation`: where Emmet expansions are suggested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmmetSuggest {
    Never,
    Always,
    /// Not in JSX (a script language with markup inside).
    MarkupAndStylesheets,
}

/// `editor.defaultColorDecorators`: when colors found in the text itself get swatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultColors {
    /// Only when no language server provides colors.
    Auto,
    Always,
    Never,
}

/// `editor.lightbulb.enabled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lightbulb {
    Off,
    /// Only on lines with code (not blank ones).
    OnCode,
    On,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoSave {
    Off,
    AfterDelay,
    OnFocusChange,
    OnWindowChange,
}

/// `editor.guides.bracketPairs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BracketGuides {
    Off,
    All,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    pub font_size: f32,
    pub line_height: f32,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub cursor_style: CursorStyle,
    pub cursor_width: f32,
    pub cursor_blink: bool,
    pub line_numbers: LineNumbers,
    pub line_highlight: LineHighlight,
    pub whitespace: Whitespace,
    pub auto_close: AutoClose,
    pub indent_guides: bool,
    /// `editor.guides.bracketPairs`: all pairs, only the active one, or none.
    pub bracket_guides: BracketGuides,
    pub scroll_beyond_last_line: bool,
    pub minimap: bool,
    /// `editor.stickyScroll.enabled` and `maxLineCount`.
    pub sticky_scroll: bool,
    pub sticky_lines: usize,
    pub minimap_slider_always: bool,
    /// Git change bars in the editor gutter.
    pub scm_gutter: bool,
    pub auto_save: AutoSave,
    pub auto_save_delay_ms: u64,
    pub insert_final_newline: bool,
    pub trim_final_newlines: bool,
    pub trim_trailing_whitespace: bool,
    pub tree_indent: f32,
    pub terminal_font_size: f32,
    pub terminal_line_height: f32,
    pub folding: bool,
    pub bracket_colors: bool,
    pub match_brackets: bool,
    /// `editor.linkedEditing`.
    pub linked_editing: bool,
    /// `editor.colorDecorators`, `editor.defaultColorDecorators` and `editor.colorDecoratorsLimit`.
    pub color_decorators: bool,
    pub default_colors: DefaultColors,
    pub color_limit: usize,
    /// `emmet.showExpandedAbbreviation` and `emmet.triggerExpansionOnTab`.
    pub emmet_suggest: EmmetSuggest,
    pub emmet_tab: bool,
    /// `languageServers.stopWhenIdle` and `languageServers.idleMinutes`.
    pub idle_stop: crate::servers::IdleStop,
    pub idle_after: std::time::Duration,
    pub fold_controls: FoldControls,
    pub lightbulb: Lightbulb,
    pub word_wrap: WordWrap,
    pub wrapping_indent: crate::layout::WrapIndent,
    /// Seconds between automatic fetches (`git.autofetch`), if enabled.
    pub git_autofetch_secs: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: 12.0,
            line_height: 18.0,
            tab_size: 4,
            insert_spaces: true,
            cursor_style: CursorStyle::Line,
            cursor_width: 2.0,
            cursor_blink: true,
            line_numbers: LineNumbers::On,
            line_highlight: LineHighlight::Line,
            whitespace: Whitespace::Selection,
            auto_close: AutoClose::Always,
            indent_guides: true,
            bracket_guides: BracketGuides::Off,
            scroll_beyond_last_line: true,
            minimap: true,
            sticky_scroll: true,
            sticky_lines: 5,
            minimap_slider_always: false,
            scm_gutter: true,
            auto_save: AutoSave::Off,
            auto_save_delay_ms: 1000,
            insert_final_newline: false,
            trim_final_newlines: false,
            trim_trailing_whitespace: false,
            tree_indent: 8.0,
            terminal_font_size: 12.0,
            terminal_line_height: 16.0,
            git_autofetch_secs: None,
            folding: true,
            bracket_colors: true,
            match_brackets: true,
            linked_editing: false,
            color_decorators: true,
            default_colors: DefaultColors::Auto,
            color_limit: 500,
            emmet_suggest: EmmetSuggest::Always,
            emmet_tab: false,
            idle_stop: crate::servers::IdleStop::Light,
            idle_after: std::time::Duration::from_secs(600),
            fold_controls: FoldControls::MouseOver,
            lightbulb: Lightbulb::OnCode,
            word_wrap: WordWrap::Off,
            wrapping_indent: crate::layout::WrapIndent::Same,
        }
    }
}

impl Config {
    pub fn from_store(s: &Store) -> Self {
        let font_size = s.number("editor.fontSize") as f32;
        // 0 derives the height from the font size (x1.5 on macOS), values below 8
        // multiply the font size, larger values are pixels.
        let lh = s.number("editor.lineHeight") as f32;
        let line_height = if lh == 0.0 {
            (font_size * 1.5).round()
        } else if lh < 8.0 {
            (font_size * lh).round()
        } else {
            lh.round()
        };
        let terminal_font_size = s.number("terminal.integrated.fontSize") as f32;
        let cursor_style = match s.string("editor.cursorStyle").as_str() {
            "block" => CursorStyle::Block,
            "underline" => CursorStyle::Underline,
            "line-thin" => CursorStyle::LineThin,
            "block-outline" => CursorStyle::BlockOutline,
            "underline-thin" => CursorStyle::UnderlineThin,
            _ => CursorStyle::Line,
        };
        let cursor_width = match s.number("editor.cursorWidth") {
            w if w > 0.0 => w as f32,
            _ => 2.0,
        };
        Self {
            font_size,
            line_height: line_height.max(font_size.ceil()),
            tab_size: s.number("editor.tabSize") as usize,
            insert_spaces: s.bool("editor.insertSpaces"),
            cursor_style,
            cursor_width,
            cursor_blink: s.string("editor.cursorBlinking") != "solid",
            line_numbers: match s.string("editor.lineNumbers").as_str() {
                "off" => LineNumbers::Off,
                "relative" => LineNumbers::Relative,
                "interval" => LineNumbers::Interval,
                _ => LineNumbers::On,
            },
            line_highlight: match s.string("editor.renderLineHighlight").as_str() {
                "none" => LineHighlight::None,
                "gutter" => LineHighlight::Gutter,
                "all" => LineHighlight::All,
                _ => LineHighlight::Line,
            },
            whitespace: match s.string("editor.renderWhitespace").as_str() {
                "none" => Whitespace::None,
                "boundary" => Whitespace::Boundary,
                "trailing" => Whitespace::Trailing,
                "all" => Whitespace::All,
                _ => Whitespace::Selection,
            },
            auto_close: match s.string("editor.autoClosingBrackets").as_str() {
                "never" => AutoClose::Never,
                "beforeWhitespace" => AutoClose::BeforeWhitespace,
                _ => AutoClose::Always,
            },
            indent_guides: s.bool("editor.guides.indentation"),
            bracket_guides: match s.get("editor.guides.bracketPairs") {
                serde_json::Value::Bool(true) => BracketGuides::All,
                serde_json::Value::String(v) if v == "true" => BracketGuides::All,
                serde_json::Value::String(v) if v == "active" => BracketGuides::Active,
                _ => BracketGuides::Off,
            },
            scroll_beyond_last_line: s.bool("editor.scrollBeyondLastLine"),
            minimap: s.bool("editor.minimap.enabled"),
            sticky_scroll: s.bool("editor.stickyScroll.enabled"),
            sticky_lines: s.number("editor.stickyScroll.maxLineCount") as usize,
            minimap_slider_always: s.string("editor.minimap.showSlider") == "always",
            scm_gutter: matches!(s.string("scm.diffDecorations").as_str(), "all" | "gutter"),
            auto_save: match s.string("files.autoSave").as_str() {
                "afterDelay" => AutoSave::AfterDelay,
                "onFocusChange" => AutoSave::OnFocusChange,
                "onWindowChange" => AutoSave::OnWindowChange,
                _ => AutoSave::Off,
            },
            auto_save_delay_ms: s.number("files.autoSaveDelay") as u64,
            insert_final_newline: s.bool("files.insertFinalNewline"),
            trim_final_newlines: s.bool("files.trimFinalNewlines"),
            trim_trailing_whitespace: s.bool("files.trimTrailingWhitespace"),
            tree_indent: s.number("workbench.tree.indent") as f32,
            terminal_font_size,
            // Line height 1 gives the cell height we've always used (4/3 of the font size).
            terminal_line_height: (terminal_font_size * 4.0 / 3.0 * s.number("terminal.integrated.lineHeight") as f32).round(),
            git_autofetch_secs: s.bool("git.autofetch").then(|| s.number("git.autofetchPeriod") as u64),
            folding: s.bool("editor.folding"),
            bracket_colors: s.bool("editor.bracketPairColorization.enabled"),
            match_brackets: s.string("editor.matchBrackets") != "never",
            linked_editing: s.bool("editor.linkedEditing"),
            color_decorators: s.bool("editor.colorDecorators"),
            emmet_suggest: match s.string("emmet.showExpandedAbbreviation").as_str() {
                "never" => EmmetSuggest::Never,
                "inMarkupAndStylesheetFilesOnly" => EmmetSuggest::MarkupAndStylesheets,
                _ => EmmetSuggest::Always,
            },
            emmet_tab: s.bool("emmet.triggerExpansionOnTab"),
            idle_stop: crate::servers::IdleStop::parse(&s.string("languageServers.stopWhenIdle")),
            idle_after: std::time::Duration::from_secs(s.number("languageServers.idleMinutes").max(1.0) as u64 * 60),
            default_colors: match s.string("editor.defaultColorDecorators").as_str() {
                "always" => DefaultColors::Always,
                "never" => DefaultColors::Never,
                _ => DefaultColors::Auto,
            },
            color_limit: s.number("editor.colorDecoratorsLimit").max(1.0) as usize,
            fold_controls: match s.string("editor.showFoldingControls").as_str() {
                "always" => FoldControls::Always,
                "never" => FoldControls::Never,
                _ => FoldControls::MouseOver,
            },
            lightbulb: match s.string("editor.lightbulb.enabled").as_str() {
                "off" => Lightbulb::Off,
                "on" => Lightbulb::On,
                _ => Lightbulb::OnCode,
            },
            word_wrap: {
                let col = s.number("editor.wordWrapColumn") as usize;
                match s.string("editor.wordWrap").as_str() {
                    "on" => WordWrap::Viewport,
                    "wordWrapColumn" => WordWrap::Column(col),
                    "bounded" => WordWrap::Bounded(col),
                    _ => WordWrap::Off,
                }
            },
            wrapping_indent: match s.string("editor.wrappingIndent").as_str() {
                "none" => crate::layout::WrapIndent::None,
                "indent" => crate::layout::WrapIndent::Indent,
                "deepIndent" => crate::layout::WrapIndent::DeepIndent,
                _ => crate::layout::WrapIndent::Same,
            },
        }
    }
}

thread_local! {
    static CONFIG: Cell<Config> = Cell::new(Config::default());
}

pub fn get() -> Config {
    CONFIG.with(Cell::get)
}

pub fn set(config: Config) {
    CONFIG.with(|c| c.set(config));
}
