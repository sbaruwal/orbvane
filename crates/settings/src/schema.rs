//! The settings orbvane understands, with the keys, defaults and descriptions.

/// How a setting's value is edited and validated.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Bool,
    Number { min: f64, max: f64, integer: bool },
    String,
    /// One of fixed values, each with a description.
    Enum(&'static [(&'static str, &'static str)]),
    /// A color theme name (the choices are the installed themes).
    Theme,
    /// A list or object from an extension, edited in settings.json; holds the property's JSON
    /// schema.
    Json(&'static str),
}

/// Where a setting appears in the Settings editor's table of contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    TextEditor,
    Cursor,
    Font,
    Minimap,
    Files,
    Workbench,
    Appearance,
    Search,
    Terminal,
    SourceControl,
    Testing,
    LanguageServers,
    Assistant,
    Extensions,
    Update,
    Emmet,
    Git,
    Html,
    Json,
    LldbDap,
    /// An extension's settings (an index into the extension sections, see `extend`).
    Extension(u16),
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Section::TextEditor => "Text Editor",
            Section::Cursor => "Cursor",
            Section::Font => "Font",
            Section::Minimap => "Minimap",
            Section::Files => "Files",
            Section::Workbench => "Workbench",
            Section::Appearance => "Appearance",
            Section::Search => "Search",
            Section::Terminal => "Terminal",
            Section::SourceControl => "Source Control",
            Section::Testing => "Testing",
            Section::LanguageServers => "Language Servers",
            Section::Assistant => "Assistant",
            Section::Extensions => "Extensions",
            Section::Update => "Update",
            Section::Emmet => "Emmet",
            Section::Git => "Git",
            Section::Html => "HTML",
            Section::Json => "JSON",
            Section::LldbDap => "LLDB DAP",
            Section::Extension(i) => EXTENSION_SECTIONS.read().unwrap_or_else(|e| e.into_inner())[i as usize],
        }
    }
}

pub struct Setting {
    pub key: &'static str,
    /// Default value as JSON.
    pub default: &'static str,
    pub kind: Kind,
    pub section: Section,
    pub description: &'static str,
}

impl Setting {
    fn clone_static(&self) -> Setting {
        Setting { key: self.key, default: self.default, kind: self.kind, section: self.section, description: self.description }
    }

    pub fn default_value(&self) -> serde_json::Value {
        serde_json::from_str(self.default).expect("valid default")
    }

    /// The display title split into category and name: `editor.minimap.enabled` →
    /// ("Editor › Minimap: ", "Enabled").
    pub fn title(&self) -> (String, String) {
        let parts: Vec<&str> = self.key.split('.').collect();
        let (last, rest) = parts.split_last().unwrap();
        let category = rest.iter().map(|p| humanize(p)).collect::<Vec<_>>().join(" › ");
        (format!("{category}: "), humanize(last))
    }

    /// Whether `value` is valid for this setting.
    pub fn accepts(&self, value: &serde_json::Value) -> bool {
        match self.kind {
            Kind::Bool => value.is_boolean(),
            Kind::Number { min, max, integer } => {
                value.as_f64().is_some_and(|n| n >= min && n <= max && (!integer || n.fract() == 0.0))
            }
            Kind::String | Kind::Theme => value.is_string(),
            Kind::Json(_) => true,
            // Options "true"/"false" are JSON booleans.
            Kind::Enum(options) => {
                let s = value.as_bool().map(|b| b.to_string()).or_else(|| value.as_str().map(str::to_string));
                s.is_some_and(|s| options.iter().any(|(o, _)| *o == s))
            }
        }
    }
}

/// `fontSize` → "Font Size", `scm` → "SCM".
fn humanize(segment: &str) -> String {
    const ACRONYMS: &[&str] = &["scm", "json", "css", "html", "url", "ui"];
    if ACRONYMS.contains(&segment) {
        return segment.to_uppercase();
    }
    let mut out = String::new();
    for (i, c) in segment.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            out.push(' ');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Settings shown under "Commonly Used", in the order.
pub const COMMONLY_USED: &[&str] = &[
    "files.autoSave",
    "editor.fontSize",
    "editor.fontFamily",
    "workbench.interfaceFont",
    "editor.tabSize",
    "editor.renderWhitespace",
    "editor.cursorStyle",
    "editor.insertSpaces",
    "editor.wordWrap",
    "workbench.colorTheme",
];

const NUM: fn(f64, f64) -> Kind = |min, max| Kind::Number { min, max, integer: false };
const INT: fn(f64, f64) -> Kind = |min, max| Kind::Number { min, max, integer: true };

/// Every setting: the built-in ones, then the ones extensions added (see `extend`).
pub fn all() -> &'static [Setting] {
    let current = *SETTINGS.read().unwrap_or_else(|e| e.into_inner());
    if !current.is_empty() {
        return current;
    }
    let builtin: &'static [Setting] = Box::leak(builtin().into_boxed_slice());
    let mut w = SETTINGS.write().unwrap_or_else(|e| e.into_inner());
    if w.is_empty() {
        *w = builtin;
    }
    *w
}

fn builtin() -> Vec<Setting> {
    {
        vec![
            // Text Editor
            Setting {
                key: "editor.tabSize",
                default: "4",
                kind: INT(1.0, 100.0),
                section: Section::TextEditor,
                description: "The number of spaces a tab is equal to.",
            },
            Setting {
                key: "editor.insertSpaces",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Insert spaces when pressing `Tab`.",
            },
            Setting {
                key: "editor.lineNumbers",
                default: "\"on\"",
                kind: Kind::Enum(&[
                    ("off", "Line numbers are not rendered."),
                    ("on", "Line numbers are rendered as absolute number."),
                    ("relative", "Line numbers are rendered as distance in lines to cursor position."),
                    ("interval", "Line numbers are rendered every 10 lines."),
                ]),
                section: Section::TextEditor,
                description: "Controls the display of line numbers.",
            },
            Setting {
                key: "editor.renderLineHighlight",
                default: "\"line\"",
                kind: Kind::Enum(&[
                    ("none", ""),
                    ("gutter", ""),
                    ("line", ""),
                    ("all", "Highlights both the gutter and the current line."),
                ]),
                section: Section::TextEditor,
                description: "Controls how the editor should render the current line highlight.",
            },
            Setting {
                key: "editor.renderWhitespace",
                default: "\"selection\"",
                kind: Kind::Enum(&[
                    ("none", ""),
                    ("boundary", "Render whitespace characters except for single spaces between words."),
                    ("selection", "Render whitespace characters only on selected text."),
                    ("trailing", "Render only trailing whitespace characters."),
                    ("all", ""),
                ]),
                section: Section::TextEditor,
                description: "Controls how the editor should render whitespace characters.",
            },
            Setting {
                key: "editor.autoClosingBrackets",
                default: "\"languageDefined\"",
                kind: Kind::Enum(&[
                    ("always", ""),
                    ("languageDefined", "Use language configurations to determine when to autoclose brackets."),
                    ("beforeWhitespace", "Autoclose brackets only when the cursor is to the left of whitespace."),
                    ("never", ""),
                ]),
                section: Section::TextEditor,
                description: "Controls whether the editor should automatically close brackets after the user adds an opening bracket.",
            },
            Setting {
                key: "editor.guides.indentation",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor should render indent guides.",
            },
            Setting {
                key: "editor.guides.bracketPairs",
                default: "false",
                kind: Kind::Enum(&[
                    ("true", "Enables bracket pair guides."),
                    ("active", "Enables bracket pair guides only for the active bracket pair."),
                    ("false", "Disables bracket pair guides."),
                ]),
                section: Section::TextEditor,
                description: "Controls whether bracket pair guides are enabled or not.",
            },
            Setting {
                key: "editor.scrollBeyondLastLine",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor will scroll beyond the last line.",
            },
            Setting {
                key: "editor.formatOnSave",
                default: "false",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Format a file on save. A formatter must be available and the editor must not be shutting down.",
            },
            Setting {
                key: "editor.bracketPairColorization.enabled",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether bracket pair colorization is enabled or not. Use `#workbench.colorCustomizations#` to override the bracket highlight colors.",
            },
            Setting {
                key: "editor.matchBrackets",
                default: "\"always\"",
                kind: Kind::Enum(&[("always", ""), ("near", ""), ("never", "")]),
                section: Section::TextEditor,
                description: "Highlight matching brackets.",
            },
            Setting {
                key: "editor.folding",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor has code folding enabled.",
            },
            Setting {
                key: "editor.foldingStrategy",
                default: "\"auto\"",
                kind: Kind::Enum(&[
                    ("auto", "Use a language-specific folding strategy if available, else the indentation-based one."),
                    ("indentation", "Use the indentation-based folding strategy."),
                ]),
                section: Section::TextEditor,
                description: "Controls the strategy for computing folding ranges.",
            },
            Setting {
                key: "editor.showFoldingControls",
                default: "\"mouseover\"",
                kind: Kind::Enum(&[
                    ("always", "Always show the folding controls."),
                    ("never", "Never show the folding controls and reduce the gutter size."),
                    ("mouseover", "Only show the folding controls when the mouse is over the gutter."),
                ]),
                section: Section::TextEditor,
                description: "Controls when the folding controls on the gutter are shown.",
            },
            Setting {
                key: "editor.parameterHints.enabled",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Enables a pop-up that shows parameter documentation and type information as you type.",
            },
            Setting {
                key: "editor.semanticHighlighting.enabled",
                default: "\"configuredByTheme\"",
                kind: Kind::Enum(&[
                    ("true", "Semantic highlighting enabled for all color themes."),
                    ("false", "Semantic highlighting disabled for all color themes."),
                    ("configuredByTheme", "Semantic highlighting is configured by the current color theme's `semanticHighlighting` setting."),
                ]),
                section: Section::TextEditor,
                description: "Controls whether the semanticHighlighting is shown for the languages that support it.",
            },
            Setting {
                key: "editor.inlayHints.enabled",
                default: "\"on\"",
                kind: Kind::Enum(&[
                    ("on", "Inlay hints are enabled"),
                    ("onUnlessPressed", "Inlay hints are showing by default and hide when holding Ctrl+Option"),
                    ("offUnlessPressed", "Inlay hints are hidden by default and show when holding Ctrl+Option"),
                    ("off", "Inlay hints are disabled"),
                ]),
                section: Section::TextEditor,
                description: "Enables the inlay hints in the editor.",
            },
            Setting {
                key: "editor.colorDecorators",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor should render the inline color decorators and color picker.",
            },
            Setting {
                key: "editor.defaultColorDecorators",
                default: "\"auto\"",
                kind: Kind::Enum(&[
                    ("auto", "Show default color decorators only when no extension provides colors decorators."),
                    ("always", "Always show default color decorators."),
                    ("never", "Never show default color decorators."),
                ]),
                section: Section::TextEditor,
                description: "Controls whether inline color decorations should be shown using the default document color provider.",
            },
            Setting {
                key: "editor.colorDecoratorsLimit",
                default: "500",
                kind: Kind::Number { min: 1.0, max: 1000000.0, integer: true },
                section: Section::TextEditor,
                description: "Controls the max number of color decorators that can be rendered in an editor at once.",
            },
            Setting {
                key: "editor.linkedEditing",
                default: "false",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor has linked editing enabled. Depending on the language, related symbols such as HTML tags, are updated while editing.",
            },
            Setting {
                key: "editor.lightbulb.enabled",
                default: "\"onCode\"",
                kind: Kind::Enum(&[
                    ("off", "Disable the code action menu."),
                    ("onCode", "Show the code action menu when the cursor is on lines with code."),
                    ("on", "Show the code action menu when the cursor is on lines with code or on empty lines."),
                ]),
                section: Section::TextEditor,
                description: "Enables the Code Action lightbulb in the editor.",
            },
            Setting {
                key: "editor.wordWrap",
                default: "\"off\"",
                kind: Kind::Enum(&[
                    ("off", "Lines will never wrap."),
                    ("on", "Lines will wrap at the viewport width."),
                    ("wordWrapColumn", "Lines will wrap at `#editor.wordWrapColumn#`."),
                    ("bounded", "Lines will wrap at the minimum of viewport and `#editor.wordWrapColumn#`."),
                ]),
                section: Section::TextEditor,
                description: "Controls how lines should wrap.",
            },
            Setting {
                key: "editor.wordWrapColumn",
                default: "80",
                kind: Kind::Number { min: 1.0, max: 10000.0, integer: true },
                section: Section::TextEditor,
                description: "Controls the wrapping column of the editor when `#editor.wordWrap#` is `wordWrapColumn` or `bounded`.",
            },
            Setting {
                key: "editor.wrappingIndent",
                default: "\"same\"",
                kind: Kind::Enum(&[
                    ("none", "No indentation. Wrapped lines begin at column 1."),
                    ("same", "Wrapped lines get the same indentation as the parent."),
                    ("indent", "Wrapped lines get +1 indentation toward the parent."),
                    ("deepIndent", "Wrapped lines get +2 indentation toward the parent."),
                ]),
                section: Section::TextEditor,
                description: "Controls the indentation of wrapped lines.",
            },
            // Cursor
            Setting {
                key: "editor.cursorBlinking",
                default: "\"blink\"",
                kind: Kind::Enum(&[("blink", ""), ("smooth", ""), ("phase", ""), ("expand", ""), ("solid", "")]),
                section: Section::Cursor,
                description: "Control the cursor animation style.",
            },
            Setting {
                key: "editor.cursorStyle",
                default: "\"line\"",
                kind: Kind::Enum(&[
                    ("line", ""),
                    ("block", ""),
                    ("underline", ""),
                    ("line-thin", ""),
                    ("block-outline", ""),
                    ("underline-thin", ""),
                ]),
                section: Section::Cursor,
                description: "Controls the cursor style in insert input mode.",
            },
            Setting {
                key: "editor.cursorWidth",
                default: "0",
                kind: INT(0.0, 20.0),
                section: Section::Cursor,
                description: "Controls the width of the cursor when `#editor.cursorStyle#` is set to `line`.",
            },
            // Font
            Setting {
                key: "editor.fontFamily",
                default: "\"SF Mono, Menlo, Monaco, 'Courier New', monospace\"",
                kind: Kind::String,
                section: Section::Font,
                description: "Controls the font family.",
            },
            Setting {
                key: "editor.fontSize",
                default: "12",
                kind: NUM(6.0, 100.0),
                section: Section::Font,
                description: "Controls the font size in pixels.",
            },
            Setting {
                key: "editor.lineHeight",
                default: "0",
                kind: NUM(0.0, 150.0),
                section: Section::Font,
                description: "Controls the line height. Use 0 to automatically compute the line height from the font size. Values between 0 and 8 will be used as a multiplier with the font size. Values greater than or equal to 8 will be used as effective values.",
            },
            Setting {
                key: "editor.formatOnType",
                default: "false",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor should automatically format the line after typing.",
            },
            Setting {
                key: "editor.links",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor should detect links and make them clickable.",
            },
            Setting {
                key: "editor.codeLens",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Controls whether the editor shows CodeLens.",
            },
            Setting {
                key: "editor.stickyScroll.enabled",
                default: "true",
                kind: Kind::Bool,
                section: Section::TextEditor,
                description: "Shows the nested current scopes during the scroll at the top of the editor.",
            },
            Setting {
                key: "editor.stickyScroll.maxLineCount",
                default: "5",
                kind: INT(1.0, 20.0),
                section: Section::TextEditor,
                description: "Defines the maximum number of sticky lines to show.",
            },
            // Minimap
            Setting {
                key: "editor.minimap.enabled",
                default: "true",
                kind: Kind::Bool,
                section: Section::Minimap,
                description: "Controls whether the minimap is shown.",
            },
            Setting {
                key: "editor.minimap.showSlider",
                default: "\"mouseover\"",
                kind: Kind::Enum(&[
                    ("always", "Always show the minimap slider."),
                    ("mouseover", "Show the minimap slider when the mouse is over the minimap."),
                ]),
                section: Section::Minimap,
                description: "Controls when the minimap slider is shown.",
            },
            // Files
            Setting {
                key: "files.autoSave",
                default: "\"off\"",
                kind: Kind::Enum(&[
                    ("off", "An editor with changes is never automatically saved."),
                    ("afterDelay", "An editor with changes is automatically saved after the configured `#files.autoSaveDelay#`."),
                    ("onFocusChange", "An editor with changes is automatically saved when the editor loses focus."),
                    ("onWindowChange", "An editor with changes is automatically saved when the window loses focus."),
                ]),
                section: Section::Files,
                description: "Controls auto save of editors that have unsaved changes.",
            },
            Setting {
                key: "files.autoSaveDelay",
                default: "1000",
                kind: INT(0.0, 1e9),
                section: Section::Files,
                description: "Controls the delay in milliseconds after which an editor with unsaved changes is saved automatically. Only applies when `#files.autoSave#` is set to `afterDelay`.",
            },
            Setting {
                key: "files.insertFinalNewline",
                default: "false",
                kind: Kind::Bool,
                section: Section::Files,
                description: "When enabled, insert a final new line at the end of the file when saving it.",
            },
            Setting {
                key: "files.trimFinalNewlines",
                default: "false",
                kind: Kind::Bool,
                section: Section::Files,
                description: "When enabled, will trim all new lines after the final new line at the end of the file when saving it.",
            },
            Setting {
                key: "files.trimTrailingWhitespace",
                default: "false",
                kind: Kind::Bool,
                section: Section::Files,
                description: "When enabled, will trim trailing whitespace when saving a file.",
            },
            // Workbench
            Setting {
                key: "files.refactoring.autoSave",
                default: "true",
                kind: Kind::Bool,
                section: Section::Files,
                description: "Controls if files that were part of a refactoring are saved automatically.",
            },
            Setting {
                key: "files.hotExit",
                default: "\"onExit\"",
                kind: Kind::Enum(&[
                    ("off", "Disable hot exit. A prompt will show when attempting to close a window with editors that have unsaved changes."),
                    ("onExit", "Hot exit will be triggered when the application is closed, that is when the window is closed or when the `workbench.action.quit` command is triggered (command palette, keybinding, menu). Unsaved files are restored upon next launch."),
                ]),
                section: Section::Files,
                description: "Controls whether unsaved files are remembered between sessions, allowing the save prompt when exiting the editor to be skipped.",
            },
            Setting {
                key: "breadcrumbs.symbolPath",
                default: "\"on\"",
                kind: Kind::Enum(&[
                    ("on", "Show all symbols in the breadcrumbs view."),
                    ("off", "Do not show symbols in the breadcrumbs view."),
                    ("last", "Only show the current symbol in the breadcrumbs view."),
                ]),
                section: Section::Workbench,
                description: "Controls whether and how symbols are shown in the breadcrumbs view.",
            },
            Setting {
                key: "workbench.editor.enablePreview",
                default: "true",
                kind: Kind::Bool,
                section: Section::Workbench,
                description: "Controls whether opened editors show as preview editors. Preview editors do not stay open, are reused until explicitly set to be kept open (via double-click or editing), and show file names in italics.",
            },
            Setting {
                key: "workbench.editor.enablePreviewFromQuickOpen",
                default: "false",
                kind: Kind::Bool,
                section: Section::Workbench,
                description: "Controls whether editors opened from Quick Open show as preview editors. Preview editors do not stay open, and are reused until explicitly set to be kept open (via double-click or editing).",
            },
            Setting {
                key: "window.restoreWindows",
                default: "\"all\"",
                kind: Kind::Enum(&[
                    ("all", "Reopen all windows with their folders and editors."),
                    ("folders", "Reopen only the windows that had a folder opened."),
                    ("none", "Never reopen a window. Always start with an empty one."),
                ]),
                section: Section::Workbench,
                description: "Controls how windows are being reopened after starting for the first time. This setting has no effect when the application is already running.",
            },
            Setting {
                key: "window.openFoldersInNewWindow",
                default: "\"default\"",
                kind: Kind::Enum(&[
                    ("default", "Folders open in a new window, unless the current window has no folder yet."),
                    ("on", "Folders always open in a new window."),
                    ("off", "Folders replace the one in the current window."),
                ]),
                section: Section::Workbench,
                description: "Controls whether folders open in a new window or replace the folder in the current window (Open Folder, Open Recent, the Welcome page and the Dock menu). A window that already has the folder comes to the front instead.",
            },
            Setting {
                key: "workbench.tree.indent",
                default: "8",
                kind: INT(4.0, 40.0),
                section: Section::Workbench,
                description: "Controls tree indentation in pixels.",
            },
            Setting {
                key: "workbench.interfaceFont",
                default: "\"editor\"",
                kind: Kind::Enum(&[
                    ("editor", "The editor's font (`#editor.fontFamily#`), everywhere."),
                    ("system", "The Mac's system font."),
                ]),
                section: Section::Appearance,
                description: "The font of the window's interface: sidebars, tabs, menus, the toolbar and the status bar.",
            },
            Setting {
                key: "workbench.colorTheme",
                default: "\"Orbvane Night\"",
                kind: Kind::Theme,
                section: Section::Appearance,
                description: "Specifies the color theme used in the workbench.",
            },
            Setting {
                key: "workbench.startupEditor",
                default: "\"welcomePage\"",
                kind: Kind::Enum(&[
                    ("welcomePage", "Open the Welcome page when nothing else opens at startup."),
                    ("none", "Start without an editor."),
                ]),
                section: Section::Appearance,
                description: "Controls which editor is shown at startup, if none are restored.",
            },
            // Features
            Setting {
                key: "search.searchEditor.doubleClickBehaviour",
                default: "\"goToLocation\"",
                kind: Kind::Enum(&[
                    ("selectWord", "Double-clicking selects the word under the cursor."),
                    ("goToLocation", "Double-clicking opens the result in the active editor group."),
                    ("openLocationToSide", "Double-clicking opens the result in the editor group to the side, creating one if it does not yet exist."),
                ]),
                section: Section::Search,
                description: "Configure effect of double-clicking a result in a search editor.",
            },
            Setting {
                key: "search.searchEditor.reusePriorSearchConfiguration",
                default: "false",
                kind: Kind::Bool,
                section: Section::Search,
                description: "When enabled, new Search Editors will reuse the includes, excludes, and flags of the previously opened Search Editor.",
            },
            Setting {
                key: "search.searchEditor.defaultNumberOfContextLines",
                default: "1",
                kind: INT(0.0, 100.0),
                section: Section::Search,
                description: "The default number of surrounding context lines to use when creating new Search Editors.",
            },
            Setting {
                key: "search.searchEditor.focusResultsOnSearch",
                default: "false",
                kind: Kind::Bool,
                section: Section::Search,
                description: "When a search is triggered, focus the Search Editor results instead of the Search Editor input.",
            },
            Setting {
                key: "terminal.integrated.fontSize",
                default: "12",
                kind: NUM(6.0, 100.0),
                section: Section::Terminal,
                description: "Controls the font size in pixels of the terminal.",
            },
            Setting {
                key: "terminal.integrated.lineHeight",
                default: "1",
                kind: NUM(1.0, 3.0),
                section: Section::Terminal,
                description: "Controls the line height of the terminal. This number is multiplied by the terminal font size to get the actual line-height in pixels.",
            },
            Setting {
                key: "scm.diffDecorations",
                default: "\"all\"",
                kind: Kind::Enum(&[
                    ("all", "Show the diff decorations in all available locations."),
                    ("gutter", "Show the diff decorations only in the editor gutter."),
                    ("overview", "Show the diff decorations only in the overview ruler."),
                    ("minimap", "Show the diff decorations only in the minimap."),
                    ("none", "Do not show the diff decorations."),
                ]),
                section: Section::SourceControl,
                description: "Controls diff decorations in the editor.",
            },
            Setting {
                key: "testing.gutterEnabled",
                default: "true",
                kind: Kind::Bool,
                section: Section::Testing,
                description: "Controls whether test decorations are shown in the editor gutter.",
            },
            Setting {
                key: "testing.defaultGutterClickAction",
                default: "\"run\"",
                kind: Kind::Enum(&[
                    ("run", "Run the test."),
                    ("debug", "Debug the test."),
                    ("contextMenu", "Open the context menu for more options."),
                ]),
                section: Section::Testing,
                description: "Controls the action to take when left-clicking on a test decoration in the gutter.",
            },
            Setting {
                key: "testing.openTesting",
                default: "\"openOnTestStart\"",
                kind: Kind::Enum(&[
                    ("neverOpen", "Never automatically open the testing views"),
                    ("openOnTestStart", "Open the test results view when tests start"),
                    ("openOnTestFailure", "Open the test result view on any test failure"),
                    ("openExplorerOnTestStart", "Open the test explorer when tests start"),
                ]),
                section: Section::Testing,
                description: "Controls when the testing view should open.",
            },
            Setting {
                key: "testing.countBadge",
                default: "\"failed\"",
                kind: Kind::Enum(&[
                    ("failed", "Show the number of failed tests"),
                    ("off", "Disable the testing count badge"),
                    ("passed", "Show the number of passed tests"),
                    ("skipped", "Show the number of skipped tests"),
                ]),
                section: Section::Testing,
                description: "Controls the count badge on the Testing icon on the Activity Bar.",
            },
            Setting {
                key: "languageServers.stopWhenIdle",
                default: "\"light\"",
                kind: Kind::Enum(&[
                    ("off", "Keep every language server running until Orbvane quits."),
                    ("light", "Stop idle servers that start quickly. Servers that take long to load a project (rust-analyzer, jdtls...) keep running."),
                    ("all", "Stop every idle server, including the ones that take long to load a project."),
                ]),
                section: Section::LanguageServers,
                description: "Controls whether language servers stop when no file they handle has been shown for a while, to free their memory. A stopped server starts again when one of its files is shown.",
            },
            Setting {
                key: "languageServers.idleMinutes",
                default: "10",
                kind: INT(1.0, 1440.0),
                section: Section::LanguageServers,
                description: "How many minutes a language server has to be idle before it stops (see Stop When Idle).",
            },
            Setting {
                key: "assistant.agent",
                default: "\"none\"",
                kind: Kind::Enum(&[
                    ("none", "No agent: the Assistant shows the agents to choose from."),
                    ("claude-code", "Claude Code, through its `claude` command line tool."),
                    ("codex", "Codex, through its `codex` command line tool."),
                    ("custom", "Any agent that speaks the Agent Client Protocol, started with Agent Command."),
                ]),
                section: Section::Assistant,
                description: "The coding agent the Assistant talks to. Claude Code and Codex need their command line tools installed and signed in; the Assistant offers to do both.",
            },
            Setting {
                key: "assistant.agent.command",
                default: "\"\"",
                kind: Kind::String,
                section: Section::Assistant,
                description: "With Agent set to custom: the command line that starts an agent that speaks the Agent Client Protocol over its standard input and output. It runs through your login shell in the open folder.",
            },
            Setting {
                key: "assistant.permissions",
                default: "\"ask\"",
                kind: Kind::Enum(&[
                    ("ask", "Ask before editing files and running commands."),
                    ("edits", "Edit files without asking; ask before running commands (Claude Code; Codex asks)."),
                    ("auto", "Work in the folder without asking; ask before going outside it."),
                    ("plan", "Read and plan only; change nothing (Codex: read only)."),
                    ("full", "Edit and run anything without asking. Use only where that's safe."),
                ]),
                section: Section::Assistant,
                description: "The permission mode new chats start in. Each chat can switch it from its header.",
            },
            Setting {
                key: "assistant.saveBeforeSending",
                default: "true",
                kind: Kind::Bool,
                section: Section::Assistant,
                description: "Controls whether files in the folder with unsaved changes are saved before each message to Claude Code or Codex, which read files from disk (other agents read them through the editor, unsaved changes included).",
            },
            Setting {
                key: "assistant.sendActiveFile",
                default: "true",
                kind: Kind::Bool,
                section: Section::Assistant,
                description: "Controls whether each message tells the agent which file is open and what's selected in it.",
            },
            Setting {
                key: "assistant.editorTools",
                default: "true",
                kind: Kind::Bool,
                section: Section::Assistant,
                description: "Controls whether the agent can ask Orbvane's language servers for definitions, references, symbols and problems (offered to it as an MCP server).",
            },
            Setting {
                key: "assistant.editorFiles",
                default: "true",
                kind: Kind::Bool,
                section: Section::Assistant,
                description: "Controls whether Claude Code reads and edits files through Orbvane (with `#assistant.editorTools#`): it sees unsaved changes, and its edits to open files are undo steps.",
            },
            Setting {
                key: "extensions.autoCheckUpdates",
                default: "true",
                kind: Kind::Bool,
                section: Section::Extensions,
                description: "When enabled, automatically checks extensions for updates. If an extension has an update, it is marked as outdated in the Extensions view. The check is performed every 12 hours.",
            },
            Setting {
                key: "extensions.autoUpdate",
                default: "true",
                kind: Kind::Bool,
                section: Section::Extensions,
                description: "Controls the automatic update behavior of extensions. The updates are fetched from the Open VSX Registry.",
            },
            Setting {
                key: "update.mode",
                default: "\"default\"",
                kind: Kind::Enum(&[
                    ("none", "Disable updates."),
                    ("manual", "Disable automatic background update checks. Updates will be available if you manually check for updates."),
                    ("default", "Enable automatic update checks. Orbvane will check for updates automatically and periodically."),
                ]),
                section: Section::Update,
                description: "Configure whether you receive automatic updates. The updates are fetched from Orbvane's releases on GitHub. With updates on, Orbvane checks shortly after starting and every 12 hours, downloads a newer version in the background and offers to restart into it.",
            },
            Setting {
                key: "git.autofetch",
                default: "false",
                kind: Kind::Bool,
                section: Section::Git,
                description: "When set to true, commits will automatically be fetched from the default remote of the current Git repository.",
            },
            Setting {
                key: "git.autofetchPeriod",
                default: "180",
                kind: Kind::Number { min: 1.0, max: 86400.0, integer: true },
                section: Section::Git,
                description: "Duration in seconds between each automatic git fetch, when `#git.autofetch#` is enabled.",
            },
            Setting {
                key: "git.confirmSync",
                default: "true",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Confirm before synchronizing Git repositories.",
            },
            Setting {
                key: "git.confirmForcePush",
                default: "true",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Controls whether to ask for confirmation before force-pushing.",
            },
            Setting {
                key: "git.enableSmartCommit",
                default: "false",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Commit all changes when there are no staged changes.",
            },
            Setting {
                key: "git.mergeEditor",
                default: "false",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Open the merge editor for files that are currently under conflict.",
            },
            Setting {
                key: "git.postCommitCommand",
                default: "\"none\"",
                kind: Kind::Enum(&[
                    ("none", "Don't run any command after a commit."),
                    ("push", "Run 'git push' after a successful commit."),
                    ("sync", "Run 'git pull' and 'git push' after a successful commit."),
                ]),
                section: Section::Git,
                description: "Run a git command after a successful commit.",
            },
            Setting {
                key: "git.pruneOnFetch",
                default: "false",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Prune when fetching.",
            },
            Setting {
                key: "git.rebaseWhenSync",
                default: "false",
                kind: Kind::Bool,
                section: Section::Git,
                description: "Force Git to use rebase when running the sync command.",
            },
            Setting {
                key: "emmet.showExpandedAbbreviation",
                default: "\"always\"",
                kind: Kind::Enum(&[("never", ""), ("always", ""), ("inMarkupAndStylesheetFilesOnly", "")]),
                section: Section::Emmet,
                description: "Shows expanded Emmet abbreviations as suggestions.\nThe option \"inMarkupAndStylesheetFilesOnly\" applies to html, haml, jade, slim, xml, xsl, css, scss, sass, less and stylus.\nThe option \"always\" applies to all parts of the file regardless of markup/css.",
            },
            Setting {
                key: "emmet.triggerExpansionOnTab",
                default: "false",
                kind: Kind::Bool,
                section: Section::Emmet,
                description: "When enabled, Emmet abbreviations are expanded when pressing TAB, even when completions do not show up. When disabled, completions that show up can still be accepted by pressing TAB.",
            },
            Setting {
                key: "html.autoClosingTags",
                default: "true",
                kind: Kind::Bool,
                section: Section::Html,
                description: "Enable/disable autoclosing of HTML tags.",
            },
            Setting {
                key: "html.autoCreateQuotes",
                default: "true",
                kind: Kind::Bool,
                section: Section::Html,
                description: "Enable/disable auto creation of quotes for HTML attribute assignment. The type of quotes can be configured by `#html.completion.attributeDefaultValue#`.",
            },
            Setting {
                key: "json.schemas",
                default: "[]",
                kind: Kind::Json(r#"{ "type": "array", "items": { "type": "object", "properties": { "fileMatch": { "type": "array", "items": { "type": "string" }, "description": "File patterns: a name like `*.conf.json`, or the end of a path like `/config/app.json`." }, "url": { "type": "string", "description": "A schema URL, absolute path, or path relative to the first workspace folder." }, "schema": { "type": "object", "description": "The schema itself." } } } }"#),
                section: Section::Json,
                description: "Associate schemas to JSON files in the current project.",
            },
            Setting {
                key: "json.schemaDownload.enable",
                default: "true",
                kind: Kind::Bool,
                section: Section::Json,
                description: "When enabled, JSON schemas can be fetched from http and https locations.",
            },
            Setting {
                key: "lldb-dap.executable-path",
                default: "\"\"",
                kind: Kind::String,
                section: Section::LldbDap,
                description: "The path to the lldb-dap binary, e.g. /usr/local/bin/lldb-dap",
            },
        ]
    }
}

/// The settings, leaked so `all()` can hand out `'static` slices; adding settings replaces it
/// with a longer copy (indices stay valid).
static SETTINGS: std::sync::RwLock<&'static [Setting]> = std::sync::RwLock::new(&[]);
static EXTENSION_SECTIONS: std::sync::RwLock<Vec<&'static str>> = std::sync::RwLock::new(Vec::new());

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The section with this title, made if new.
fn extension_section(title: &str) -> Section {
    let mut sections = EXTENSION_SECTIONS.write().unwrap_or_else(|e| e.into_inner());
    let i = sections.iter().position(|s| *s == title).unwrap_or_else(|| {
        sections.push(leak(title));
        sections.len() - 1
    });
    Section::Extension(i as u16)
}

/// The sections extensions added, in order.
pub fn extension_sections() -> Vec<Section> {
    (0..EXTENSION_SECTIONS.read().unwrap_or_else(|e| e.into_inner()).len()).map(|i| Section::Extension(i as u16)).collect()
}

/// A setting from an extension's `contributes.configuration`: its JSON schema (`type`,
/// `default`, `enum`, `description`...) and the title of the block it's in.
pub fn extension_setting(key: &str, schema: &serde_json::Value, section: &str) -> Setting {
    use serde_json::Value;
    let ty = match &schema["type"] {
        Value::Array(types) => types.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("string").to_string(),
        t => t.as_str().unwrap_or("string").to_string(),
    };
    let options: Option<Vec<(&'static str, &'static str)>> = schema["enum"].as_array().map(|values| {
        let descriptions = schema["enumDescriptions"].as_array().or(schema["markdownEnumDescriptions"].as_array());
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let value = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                (leak(&value), leak(descriptions.and_then(|d| d.get(i)).and_then(Value::as_str).unwrap_or("")))
            })
            .collect()
    });
    let kind = match (ty.as_str(), options) {
        (_, Some(options)) => Kind::Enum(Box::leak(options.into_boxed_slice())),
        ("boolean", _) => Kind::Bool,
        ("number" | "integer", _) => Kind::Number {
            min: schema["minimum"].as_f64().unwrap_or(f64::MIN),
            max: schema["maximum"].as_f64().unwrap_or(f64::MAX),
            integer: ty == "integer",
        },
        ("string", _) => Kind::String,
        _ => Kind::Json(leak(&schema.to_string())),
    };
    let default = match &schema["default"] {
        Value::Null => match kind {
            Kind::Bool => "false".to_string(),
            Kind::Number { min, .. } => if min > f64::MIN { min.to_string() } else { "0".to_string() },
            Kind::String => "\"\"".to_string(),
            Kind::Enum(options) => serde_json::to_string(options.first().map_or("", |o| o.0)).unwrap(),
            _ => if ty == "array" { "[]".to_string() } else if ty == "object" { "{}".to_string() } else { "null".to_string() },
        },
        v => v.to_string(),
    };
    let description = schema["markdownDescription"].as_str().or(schema["description"].as_str()).unwrap_or("");
    Setting { key: leak(key), default: leak(&default), kind, section: extension_section(section), description: leak(description) }
}

/// Adds settings (from extensions); keys already known are skipped.
pub fn extend(new: Vec<Setting>) {
    let current = all();
    let mut list: Vec<Setting> = current.iter().map(Setting::clone_static).collect();
    for s in new {
        if !list.iter().any(|x| x.key == s.key) {
            list.push(s);
        }
    }
    if list.len() > current.len() {
        *SETTINGS.write().unwrap_or_else(|e| e.into_inner()) = Box::leak(list.into_boxed_slice());
    }
}

pub fn find(key: &str) -> Option<&'static Setting> {
    all().iter().find(|s| s.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_defaults() {
        assert_eq!(find("editor.fontSize").unwrap().title(), ("Editor: ".into(), "Font Size".into()));
        assert_eq!(find("editor.minimap.enabled").unwrap().title(), ("Editor › Minimap: ".into(), "Enabled".into()));
        assert_eq!(find("scm.diffDecorations").unwrap().title(), ("SCM: ".into(), "Diff Decorations".into()));
        assert_eq!(find("git.postCommitCommand").unwrap().title(), ("Git: ".into(), "Post Commit Command".into()));
        for s in all() {
            assert!(s.accepts(&s.default_value()), "{} default is invalid", s.key);
        }
        for key in COMMONLY_USED {
            assert!(find(key).is_some(), "{key}");
        }
    }

    #[test]
    fn extensions_add_settings() {
        let schema = serde_json::json!({ "type": "string", "enum": ["words", "chars"], "enumDescriptions": ["Words", "Characters"], "default": "words", "description": "What to count." });
        let list = serde_json::json!({ "type": "array", "items": { "type": "string" } });
        let n = all().len();
        extend(vec![
            extension_setting("wordCount.mode", &schema, "Word Count"),
            extension_setting("wordCount.ignore", &list, "Word Count"),
            extension_setting("wordCount.max", &serde_json::json!({ "type": "integer", "minimum": 1 }), "Word Count"),
            extension_setting("editor.fontSize", &list, "Word Count"),
        ]);
        assert_eq!(all().len(), n + 3);
        let mode = find("wordCount.mode").unwrap();
        assert!(matches!(mode.kind, Kind::Enum([("words", "Words"), ("chars", "Characters")])));
        assert_eq!(mode.section.label(), "Word Count");
        assert!(extension_sections().contains(&mode.section));
        assert_eq!(find("wordCount.ignore").unwrap().default_value(), serde_json::json!([]));
        assert!(matches!(find("wordCount.max").unwrap().kind, Kind::Number { min: 1.0, integer: true, .. }));
        assert_eq!(find("wordCount.max").unwrap().default_value(), serde_json::json!(1));
        assert!(matches!(find("editor.fontSize").unwrap().kind, Kind::Number { .. }));
    }
}
